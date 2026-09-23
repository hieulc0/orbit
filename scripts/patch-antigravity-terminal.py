#!/usr/bin/env python3
"""Build-only, exact-version source overlay; never modifies the supplied binary."""
import argparse
import hashlib
import importlib.util
import marshal
import pathlib
import shutil
import struct
import tempfile
import zipfile

SERVER_SHA256 = "267affa691085fe5d78895e34dffe723d6528713e01bd37ed40feb7b43d1f4c7"
MODULE = "google3/cloud/developer_experience/antigravity_extensions/acp_server/"
ADAPTER_BYTECODE = frozenset({
    MODULE + "__pycache__/server.cpython-314.pyc",
    MODULE + "__pycache__/tools.cpython-314.pyc",
})


def replace_once(source, old, new):
    if source.count(old) != 1:
        raise ValueError("Antigravity source anchor mismatch")
    return source.replace(old, new, 1)


def patch_server(source):
    source = replace_once(
        source,
        '    if client_capabilities and hasattr(client_capabilities, "fs"):',
        '    self._orbit_terminal = bool(getattr(client_capabilities, "terminal", False))\n'
        '    if client_capabilities and hasattr(client_capabilities, "fs"):',
    )
    # Native commands must not execute in the credential-bearing control HOME,
    # including when a client does not advertise terminal support.
    source = replace_once(
        source,
        '    disabled_builtin_tools = [sdk_types_module.BuiltinTools.VIEW_FILE]',
        '    disabled_builtin_tools = [sdk_types_module.BuiltinTools.VIEW_FILE,\n'
        '                              sdk_types_module.BuiltinTools.RUN_COMMAND]',
    )
    # The upstream fallback includes GEMINI_HOME in its readable directories.
    # It must not be a model-visible alternative to the jailed client reader.
    source = replace_once(
        source,
        '    session_tools.append(\n'
        '        tools.make_safe_view_file(allowed_dirs, defer_scope=admin_active)\n'
        '    )',
        '    # Orbit repository reads use the confined ACP client tools only.',
    )
    source = replace_once(
        source,
        '    session_policies = policy.safe_defaults(handler)',
        '    if getattr(self, "_orbit_terminal", False):\n'
        '      session_tools.append(tools.make_orbit_client_terminal(self._client, abs_cwd))\n'
        '\n    session_policies = policy.safe_defaults(handler)',
    )
    return source


def rebuilt_bytecode(source, filename, original):
    if original[:4] != importlib.util.MAGIC_NUMBER:
        raise ValueError("use the matching Python 3.14 bytecode compiler")
    return (original[:4] + struct.pack("<I", 3) + importlib.util.source_hash(source.encode())
            + marshal.dumps(compile(source, filename, "exec")))


def rebuild_zip(original, archive, changes):
    replacements = {name: value.encode() for name, value in changes.items()}
    for cached in ADAPTER_BYTECODE:
        name = cached.rsplit("/", 1)[-1].split(".", 1)[0]
        source_name = MODULE + name + ".py"
        replacements[cached] = rebuilt_bytecode(
            changes[source_name], source_name, original.read(cached))
    with zipfile.ZipFile(archive, "w") as patched:
        for entry in original.infolist():
            # Patch both representations; never trust a source-only overlay.
            if entry.filename in replacements:
                patched.writestr(entry, replacements[entry.filename])
            else:
                with original.open(entry) as data, patched.open(entry, "w") as dest:
                    shutil.copyfileobj(data, dest, 1024 * 1024)


def build(source, output):
    with source.open("rb") as stream:
        if hashlib.file_digest(stream, "sha256").hexdigest() != SERVER_SHA256:
            raise ValueError("Antigravity binary digest mismatch")
    extension = (pathlib.Path(__file__).resolve().parents[1] /
                 "deploy/antigravity/client_terminal.py").read_text()
    with zipfile.ZipFile(source) as original:
        if not ADAPTER_BYTECODE.issubset(original.namelist()):
            raise ValueError("Antigravity bytecode layout mismatch")
        changes = {
            MODULE + "server.py": patch_server(original.read(MODULE + "server.py").decode()),
            MODULE + "tools.py": original.read(MODULE + "tools.py").decode() + "\n\n" + extension,
        }
        for name, content in changes.items():
            compile(content, name, "exec")
        # This is an ELF .par_data section, not merely a self-extracting ZIP.
        # ZIP offsets are relative to that section; its ELF size must be updated.
        # Refuse an existing destination rather than modifying prior evidence.
        prefix_size = min(entry.header_offset for entry in original.infolist())
        with tempfile.TemporaryFile(dir=output.parent) as archive:
            rebuild_zip(original, archive, changes)
            archive_size = archive.tell()
            archive.seek(0)
            with source.open("rb") as src, output.open("xb") as dst:
                # The input digest pins these ELF64 section-table coordinates.
                section_size_offset = 621948208 + 68 * 64 + 32
                prefix = bytearray(src.read(prefix_size))
                if len(prefix) != prefix_size or prefix[:6] != b"\x7fELF\x02\x01":
                    raise ValueError("unexpected ELF prefix")
                if struct.unpack_from("<Q", prefix, section_size_offset - 8)[0] != prefix_size:
                    raise ValueError("unexpected .par_data section offset")
                struct.pack_into("<Q", prefix, section_size_offset, archive_size)
                dst.write(prefix)
                del prefix
                shutil.copyfileobj(archive, dst, 1024 * 1024)
    with zipfile.ZipFile(output) as check:
        for cached in ADAPTER_BYTECODE:
            name = cached.rsplit("/", 1)[-1].split(".", 1)[0]
            expected = importlib.util.source_hash(check.read(MODULE + name + ".py"))
            if check.read(cached)[8:16] != expected:
                raise ValueError("stale adapter bytecode retained")
    output.chmod(0o755)
    with output.open("rb") as stream:
        print(hashlib.file_digest(stream, "sha256").hexdigest())


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=pathlib.Path)
    parser.add_argument("output", type=pathlib.Path)
    args = parser.parse_args()
    build(args.source, args.output)
