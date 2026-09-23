# Appended to the verified Antigravity 1.1.1 tools module at image build time.
# Client, tool_context and Callable/Coroutine/Any come from that module.
def make_orbit_client_terminal(client, workspace_path):
  """Expose command effects only through the ACP client's existing sandbox."""
  async def orbit_terminal(command: str, ctx: tool_context.ToolContext) -> str:
    """Run a repository-local shell command in the isolated Git workspace.

    Use for Git inspection and applicable project build/test/validation commands.
    Output and execution time are bounded by the client. A nonzero exit is a
    command result, not a broken session; inspect the result and repair as needed.
    """
    import json

    if client is None:
      raise RuntimeError("ACP terminal client unavailable")
    if not command.strip() or len(command.encode("utf-8")) > 8192:
      raise ValueError("terminal command must contain 1..8192 bytes")
    session_id = ctx.conversation_id
    terminal = await client.create_terminal(
        session_id=session_id, command="sh", args=["-c", command],
        cwd=workspace_path, output_byte_limit=65536,
    )
    try:
      await client.wait_for_terminal_exit(
          session_id=session_id, terminal_id=terminal.terminal_id,
      )
      output = await client.terminal_output(
          session_id=session_id, terminal_id=terminal.terminal_id,
      )
      if len(output.output.encode("utf-8")) > 65536:
        raise RuntimeError("ACP terminal response exceeds output bound")
      # The bundled Python SDK's wait response differs from Orbit's Rust ACP
      # schema. terminal/output carries the same explicit exitStatus in both.
      exit_status = output.exit_status
      if exit_status is None:
        raise RuntimeError("ACP terminal exit unconfirmed")
      return json.dumps({
          "exit_code": exit_status.exit_code, "signal": exit_status.signal,
          "output": output.output, "truncated": output.truncated,
      })
    finally:
      # On transport failure the worker/supervisor still owns final cleanup.
      # Never fall back to the auth-bearing agent process or a host subprocess.
      await client.release_terminal(
          session_id=session_id, terminal_id=terminal.terminal_id,
      )

  return orbit_terminal
