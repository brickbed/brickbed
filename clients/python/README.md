# brickbed

Early Python client for [Brickbed](https://github.com/brickbed/brickbed), the alpha document database for local and S3-compatible object storage.

The Python client is less complete than the TypeScript client. See the repository [HTTP API](../../docs/http-api.md) for unsupported operations and the [quickstart](../../docs/quickstart.md) for the primary TypeScript workflow.

## Installation

```bash
pip install brickbed
# or
uv add brickbed
```

## Usage

```python
from brickbed import createClient

db = createClient(
    endpoint=os.environ["BRICKBED_ENDPOINT"],
    api_key=os.environ["BRICKBED_API_KEY"],
)

# Insert a document
post = await db.collection("posts").insert(
    {
        "title": "Hello World",
        "content": "My first post",
    }
)

# Get a document
fetched = await db.collection("posts").get(post["_id"])

# List documents
result = await db.collection("posts").list(limit=10)
```

# Error handling

`BrickbedError` exposes `status`, stable `code`, human `message`, optional
safe `details`, `request_id`, and raw `body`. Branch on `code`, not the text
of `message`; unknown future codes remain strings.

```python
from brickbed import BrickbedError

try:
    await posts.query("by_status", {"status": "published"}, cursor="bad")
except BrickbedError as error:
    if error.code == "invalid_cursor":
        print(error.request_id)
```

See the [HTTP error contract](../../docs/http-api.md#errors) for every code.

## License

MIT
