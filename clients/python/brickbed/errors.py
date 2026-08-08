import json
from typing import Any

import httpx


class BrickbedError(Exception):
    """A Brickbed HTTP failure.

    ``code`` is stable for programmatic handling. ``message`` is intended for
    people and may evolve. Unknown server codes are preserved as strings so a
    newer server remains diagnosable by an older SDK.
    """

    def __init__(
        self,
        status: int,
        code: str,
        message: str,
        details: dict[str, Any] | None = None,
        request_id: str | None = None,
        body: str = "",
    ):
        self.status = status
        self.code = code
        self.message = message
        self.details = details
        self.request_id = request_id
        self.body = body
        super().__init__(f"BrickbedError({status}, {code}): {message}")

    @classmethod
    def from_response(cls, response: httpx.Response) -> "BrickbedError":
        body = response.text
        code = "http_error"
        message = body or response.reason_phrase or "request failed"
        details: dict[str, Any] | None = None
        request_id = response.headers.get("x-request-id")
        try:
            parsed = json.loads(body)
            if isinstance(parsed, dict):
                if isinstance(parsed.get("requestId"), str):
                    request_id = parsed["requestId"]
                error = parsed.get("error")
                if isinstance(error, dict):
                    if isinstance(error.get("code"), str):
                        code = error["code"]
                    if isinstance(error.get("message"), str):
                        message = error["message"]
                    if isinstance(error.get("details"), dict):
                        details = error["details"]
                elif isinstance(error, str):
                    # Backwards compatibility with pre-v1 Brickbed servers.
                    message = error
        except (TypeError, ValueError):
            pass
        return cls(response.status_code, code, message, details, request_id, body)
