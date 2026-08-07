from typing import TypedDict


class Document(TypedDict, total=False):
    _id: str
    _createdAt: int
    _updatedAt: int


class ListOptions(TypedDict, total=False):
    limit: int
    cursor: str


class ListResponse(TypedDict):
    data: list[Document]
    cursor: str | None
