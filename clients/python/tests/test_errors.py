import unittest

import httpx

from brickbed import BrickbedError


class BrickbedErrorTests(unittest.TestCase):
    def test_parses_the_v1_error_envelope(self) -> None:
        response = httpx.Response(
            400,
            headers={"x-request-id": "header-request-id"},
            json={
                "error": {
                    "code": "invalid_cursor",
                    "message": "cursor does not match this query",
                    "details": {"field": "cursor"},
                },
                "requestId": "body-request-id",
            },
        )

        error = BrickbedError.from_response(response)

        self.assertEqual(error.status, 400)
        self.assertEqual(error.code, "invalid_cursor")
        self.assertEqual(error.message, "cursor does not match this query")
        self.assertEqual(error.details, {"field": "cursor"})
        self.assertEqual(error.request_id, "body-request-id")

    def test_preserves_an_unknown_future_code(self) -> None:
        response = httpx.Response(
            409,
            json={
                "error": {"code": "future_conflict", "message": "try later"},
                "requestId": "request-123",
            },
        )

        error = BrickbedError.from_response(response)

        self.assertEqual(error.code, "future_conflict")
        self.assertEqual(error.request_id, "request-123")

    def test_retains_a_plain_text_proxy_response(self) -> None:
        response = httpx.Response(502, content="gateway failed")

        error = BrickbedError.from_response(response)

        self.assertEqual(error.code, "http_error")
        self.assertEqual(error.message, "gateway failed")
        self.assertEqual(error.body, "gateway failed")


if __name__ == "__main__":
    unittest.main()
