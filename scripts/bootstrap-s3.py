#!/usr/bin/env python3
"""Bootstrap an S3 bucket for Orbit qualification using AWS Signature Version 4.

Uses only the Python standard library to ensure zero external dependencies in CI
and developer environments.
"""

import argparse
import datetime
import hashlib
import hmac
import os
import sys
import urllib.error
import urllib.parse
import urllib.request


def sign(key: bytes, msg: str) -> bytes:
    return hmac.new(key, msg.encode("utf-8"), hashlib.sha256).digest()


def get_signature_key(key: str, date_stamp: str, region: str, service: str) -> bytes:
    k_date = sign(("AWS4" + key).encode("utf-8"), date_stamp)
    k_region = sign(k_date, region)
    k_service = sign(k_region, service)
    return sign(k_service, "aws4_request")


def ensure_bucket(
    endpoint: str,
    bucket: str,
    region: str = "us-east-1",
    access_key: str = "orbit-local-test",
    secret_key: str = "orbit-local-test-secret",
) -> None:
    parsed = urllib.parse.urlparse(endpoint)
    host = parsed.netloc

    now = datetime.datetime.now(datetime.timezone.utc)
    amz_date = now.strftime("%Y%m%dT%H%M%SZ")
    date_stamp = now.strftime("%Y%m%d")

    payload = b""
    payload_hash = hashlib.sha256(payload).hexdigest()

    canonical_uri = f"/{bucket}"
    canonical_querystring = ""
    canonical_headers = (
        f"host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\n"
    )
    signed_headers = "host;x-amz-content-sha256;x-amz-date"
    canonical_request = (
        f"PUT\n{canonical_uri}\n{canonical_querystring}\n"
        f"{canonical_headers}\n{signed_headers}\n{payload_hash}"
    )

    algorithm = "AWS4-HMAC-SHA256"
    credential_scope = f"{date_stamp}/{region}/s3/aws4_request"
    string_to_sign = (
        f"{algorithm}\n{amz_date}\n{credential_scope}\n"
        f"{hashlib.sha256(canonical_request.encode('utf-8')).hexdigest()}"
    )

    signing_key = get_signature_key(secret_key, date_stamp, region, "s3")
    signature = hmac.new(
        signing_key, string_to_sign.encode("utf-8"), hashlib.sha256
    ).hexdigest()

    authorization_header = (
        f"{algorithm} Credential={access_key}/{credential_scope}, "
        f"SignedHeaders={signed_headers}, Signature={signature}"
    )

    url = f"{endpoint.rstrip('/')}/{bucket}"
    req = urllib.request.Request(
        url,
        data=payload,
        method="PUT",
        headers={
            "Host": host,
            "x-amz-date": amz_date,
            "x-amz-content-sha256": payload_hash,
            "Authorization": authorization_header,
        },
    )

    try:
        with urllib.request.urlopen(req) as resp:
            if resp.status in (200, 204):
                print(f"Bucket '{bucket}' ready (HTTP {resp.status}).")
                return
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", errors="replace")
        # BucketAlreadyOwnedByYou or BucketAlreadyExists is acceptable
        if e.code == 409 or "BucketAlready" in body:
            print(f"Bucket '{bucket}' already exists (HTTP {e.code}).")
            return
        print(f"Failed to ensure bucket '{bucket}': HTTP {e.code} {e.reason}\n{body}", file=sys.stderr)
        raise


def main() -> None:
    parser = argparse.ArgumentParser(description="Ensure an S3 bucket exists.")
    parser.add_argument(
        "--endpoint",
        default=os.environ.get("ORBIT_TEST_S3_ENDPOINT", "http://127.0.0.1:55440"),
        help="S3 endpoint URL (default: http://127.0.0.1:55440)",
    )
    parser.add_argument(
        "--bucket",
        default=os.environ.get("ORBIT_TEST_S3_BUCKET", "orbit-qualification"),
        help="S3 bucket name (default: orbit-qualification)",
    )
    parser.add_argument(
        "--region",
        default=os.environ.get("ORBIT_TEST_S3_REGION", "us-east-1"),
        help="S3 region (default: us-east-1)",
    )
    parser.add_argument(
        "--access-key",
        default=os.environ.get("ORBIT_TEST_S3_ACCESS_KEY", "orbit-local-test"),
        help="S3 access key",
    )
    parser.add_argument(
        "--secret-key",
        default=os.environ.get("ORBIT_TEST_S3_SECRET_KEY", "orbit-local-test-secret"),
        help="S3 secret key",
    )
    args = parser.parse_args()

    ensure_bucket(
        endpoint=args.endpoint,
        bucket=args.bucket,
        region=args.region,
        access_key=args.access_key,
        secret_key=args.secret_key,
    )


if __name__ == "__main__":
    main()
