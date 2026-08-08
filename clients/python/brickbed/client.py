from types import TracebackType
from typing import Any

import httpx

from brickbed.collection import Collection
from brickbed.errors import BrickbedError


class BrickbedClient:
    def __init__(self, endpoint: str, api_key: str, project_id: str):
        if not project_id:
            raise ValueError("BrickbedClient requires project_id")
        self._endpoint = endpoint.rstrip("/")
        self._api_key = api_key
        self._project_id = project_id
        self._http = httpx.AsyncClient(
            base_url=self._endpoint,
            headers={
                "Content-Type": "application/json",
                "Authorization": f"Bearer {api_key}",
            },
        )

    def collection(self, name: str) -> Collection:
        return Collection(self._http, self._project_id, name)

    async def push_schema(self, schema: dict[str, Any]) -> None:
        """Push the project schema; enables validation + indexes."""
        response = await self._http.put(f"/v1/{self._project_id}/_schema", json=schema)
        if not response.is_success:
            raise BrickbedError.from_response(response)

    async def aclose(self) -> None:
        await self._http.aclose()

    async def __aenter__(self) -> "BrickbedClient":
        return self

    async def __aexit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> None:
        await self.aclose()


def create_client(endpoint: str, api_key: str, project_id: str) -> BrickbedClient:
    return BrickbedClient(endpoint, api_key, project_id)


# Deprecated alias for the pre-0.2 camelCase name.
createClient = create_client
