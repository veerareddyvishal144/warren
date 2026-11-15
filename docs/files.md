# Files

The gateway inside every instance exposes the persistent home directory over HTTP. Paths are relative to the home (`/home/agent` on the built-in templates) and confined to it.

```bash
export WARREN_API_KEY=sk_live_...
KEY="X-Warren-Key: $WARREN_API_KEY"
URL=http://ab12cd34ef.warren.localhost:7337     # the instance's `url`
```

## Confinement

Every path is resolved against the home and must stay inside it. `..` segments, absolute paths outside the home and symlinks that point outside (or dangle) are rejected with `403 forbidden`; the home itself cannot be deleted or moved. `path=.` or an empty path is the home. URL-encode paths with spaces or special characters (`notes%20and%20plans/todo.md`).

## List a directory

```bash
curl -s "$URL/v1/files?path=." -H "$KEY"
```

```json
{
  "path": ".",
  "entries": [
    { "name": "notes", "type": "dir", "size": 4096, "modified_at": 1763000000000 },
    { "name": "plan.md", "type": "file", "size": 182, "modified_at": 1763000100000 }
  ]
}
```

`type` is `file` or `dir`; `modified_at` is unix milliseconds.

## Read a file

```bash
curl -s "$URL/v1/files/content?path=plan.md" -H "$KEY"
curl -s "$URL/v1/files/content?path=photos/cat.png" -H "$KEY" -o cat.png
```

The body is the raw bytes with a content type guessed from the name (`text/markdown`, `image/png`, `application/octet-stream`, ...). Unknown paths are `404 not_found`; directories are not readable here (list or download them instead).

## Write a file

```bash
curl -s -X PUT "$URL/v1/files/content?path=notes/todo.md" -H "$KEY" --data-binary $'- read the docs\n- ship\n'
# {"path":"notes/todo.md","size":24}
curl -s -X PUT "$URL/v1/files/content?path=data/input.csv" -H "$KEY" --data-binary @./input.csv
```

The raw request body is the file; parent directories are created; an existing file is replaced. Use `--data-binary` (not `-d`, which strips newlines) or `--upload-file` for files.

## Move or rename

```bash
curl -s -X POST "$URL/v1/files/move" -H "$KEY" -H 'Content-Type: application/json' -d '{"from":"notes/todo.md","to":"archive/todo-2026.md"}'
```

Both paths are confined to the home; parents of `to` are created.

## Delete

```bash
curl -s -X DELETE "$URL/v1/files?path=archive/todo-2026.md" -H "$KEY"      # {"path":"archive/todo-2026.md","deleted":true}
curl -s -X DELETE "$URL/v1/files?path=archive" -H "$KEY"                   # directories are removed recursively
```

## Download a directory

```bash
curl -s "$URL/v1/files/download?path=project" -H "$KEY" -o project.tar.gz
tar tzf project.tar.gz | head
```

The response is `application/gzip`, a tarball of the directory. `path=.` downloads the whole home.

## Upload a directory

There is no multipart upload; two ways work well:

```bash
# 1. one PUT per file
find ./site -type f | while read -r f; do
  curl -s -X PUT "$URL/v1/files/content?path=site/${f#./site/}" -H "$KEY" --data-binary "@$f" >/dev/null
done

# 2. a tarball through PUT, unpacked with exec
tar czf - -C ./site . | curl -s -X PUT "$URL/v1/files/content?path=site.tar.gz" -H "$KEY" --data-binary @-
curl -s -X POST http://api.warren.localhost:7337/v1/instances/ab12cd34ef/exec -H "Authorization: Bearer $WARREN_API_KEY" \
  -H 'Content-Type: application/json' -d '{"command":"mkdir -p site && tar xzf site.tar.gz -C site && rm site.tar.gz"}'
```

Request bodies are bounded (4 MiB on JSON endpoints; file bodies may be larger but very large uploads are better done with `git clone`, `curl` or `rsync` from inside the instance through exec).

## CLI

```bash
warren files ls ab12cd34ef notes          # list a directory (path optional, default the home)
warren files cat ab12cd34ef plan.md       # print a file
warren files rm ab12cd34ef archive        # delete a file or directory
warren files put --help                   # upload a local file
warren files download --help              # fetch a directory as a tarball
```

## Where the data lives

The home is a bind mount of `<data>/instances/<id>/home` on the host (a filesystem inside `rootfs.ext4` on `vm` instances). It survives sleep, stop, restart, resize and template updates; it is copied on fork, archived by backups and removed on delete. The Agent API keeps its sessions under `.warren/` in the same directory, so you can read or clear them here.

## Sleeping instances

A files request to a sleeping instance wakes it and is held until the gateway is up. Allow for a longer first request.
