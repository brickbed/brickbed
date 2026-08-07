from typing import Any

import httpx

from brickbed.errors import BrickbedError
from brickbed.types import Document, ListResponse


class Collection:
    def __init__(self, http: httpx.AsyncClient, project_id: str, name: str):
        self._http = http
        self._project_id = project_id
        self._name = name

    @property
    def _base_path(self) -> str:
        return f"/v1/{self._project_id}/{self._name}"

    async def _request(
        self,
        method: str,
        path: str = "",
        json: dict[str, Any] | None = None,
        params: dict[str, Any] | None = None,
    ) -> Any:
        response = await self._http.request(
            method,
            f"{self._base_path}{path}",
            json=json,
            params=params,
        )

        if not response.is_success:
            raise BrickbedError(response.status_code, response.text)

        if response.status_code == 204:
            return None

        return response.json()

    async def get(self, id: str) -> Document | None:
        try:
            return await self._request("GET", f"/{id}")
        except BrickbedError as e:
            if e.status == 404:
                return None
            raise

    async def insert(self, data: dict[str, Any]) -> Document:
        return await self._request("POST", "", json=data)

    async def update(self, id: str, data: dict[str, Any]) -> Document:
        return await self._request("PUT", f"/{id}", json=data)

    async def patch(self, id: str, data: dict[str, Any]) -> Document:
        return await self._request("PATCH", f"/{id}", json=data)

    async def delete(self, id: str) -> None:
        await self._request("DELETE", f"/{id}")

    async def list(
        self,
        limit: int | None = None,
        cursor: str | None = None,
    ) -> ListResponse:
        params: dict[str, Any] = {}
        if limit is not None:
            params["limit"] = limit
        if cursor is not None:
            params["cursor"] = cursor
        return await self._request("GET", "", params=params if params else None)

    async def query(
        self,
        index_name: str,
        params: dict[str, Any],
        limit: int | None = None,
        cursor: str | None = None,
    ) -> list[Document]:
        body: dict[str, Any] = {"index": index_name, "params": params}
        if limit is not None:
            body["limit"] = limit
        if cursor is not None:
            body["cursor"] = cursor
        result = await self._request("POST", "/_query", json=body)
        return result["data"]
