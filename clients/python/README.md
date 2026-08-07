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

## License

MIT
