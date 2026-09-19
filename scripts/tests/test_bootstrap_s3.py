import importlib.util
from pathlib import Path
import unittest
from unittest.mock import MagicMock, patch
import urllib.error


def module(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).parents[1] / filename)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


bootstrap = module("bootstrap_s3", "bootstrap-s3.py")


class BootstrapS3Test(unittest.TestCase):
    def test_signature_derivation_deterministic(self):
        k = bootstrap.get_signature_key("secret", "20260919", "us-east-1", "s3")
        self.assertEqual(len(k), 32)
        k2 = bootstrap.get_signature_key("secret", "20260919", "us-east-1", "s3")
        self.assertEqual(k, k2)

    @patch("urllib.request.urlopen")
    def test_ensure_bucket_success(self, mock_urlopen):
        mock_resp = MagicMock()
        mock_resp.status = 200
        mock_resp.__enter__.return_value = mock_resp
        mock_urlopen.return_value = mock_resp

        bootstrap.ensure_bucket("http://127.0.0.1:55440", "test-bucket")
        mock_urlopen.assert_called_once()
        req = mock_urlopen.call_args[0][0]
        self.assertEqual(req.get_method(), "PUT")
        self.assertEqual(req.full_url, "http://127.0.0.1:55440/test-bucket")
        self.assertIn("AWS4-HMAC-SHA256", req.headers["Authorization"])

    @patch("urllib.request.urlopen")
    def test_ensure_bucket_already_exists_409(self, mock_urlopen):
        fp = MagicMock()
        fp.read.return_value = b"<Error><Code>BucketAlreadyOwnedByYou</Code></Error>"
        mock_urlopen.side_effect = urllib.error.HTTPError(
            url="http://127.0.0.1:55440/test-bucket",
            code=409,
            msg="Conflict",
            hdrs={},
            fp=fp,
        )

        # Should return cleanly without raising
        bootstrap.ensure_bucket("http://127.0.0.1:55440", "test-bucket")

    @patch("urllib.request.urlopen")
    def test_ensure_bucket_unexpected_error_raises(self, mock_urlopen):
        fp = MagicMock()
        fp.read.return_value = b"<Error><Code>AccessDenied</Code></Error>"
        mock_urlopen.side_effect = urllib.error.HTTPError(
            url="http://127.0.0.1:55440/test-bucket",
            code=403,
            msg="Forbidden",
            hdrs={},
            fp=fp,
        )

        with self.assertRaises(urllib.error.HTTPError):
            bootstrap.ensure_bucket("http://127.0.0.1:55440", "test-bucket")


if __name__ == "__main__":
    unittest.main()
