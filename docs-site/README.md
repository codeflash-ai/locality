# Locality Docs Source

This directory is the Mintlify documentation source root.

- `docs.json` configures the docs navigation.
- `*.mdx` files are user-facing docs source files.
- `connectors/*.mdx` contains connector docs source files.
- `llms.txt` is the AI-readable docs index source.

Run from the repository root:

```sh
make docs-dev
make docs-validate
make docs-broken-links
```

Internal engineering notes belong in `../docs/`, not here.
