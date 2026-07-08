# Locality Public Docs

This directory is the Mintlify documentation site root.

- `docs.json` configures the published navigation.
- `*.mdx` files are public user-facing docs.
- `connectors/*.mdx` contains public connector docs.
- `llms.txt` is the public AI-readable docs index.

Run from the repository root:

```sh
make docs-dev
make docs-validate
make docs-broken-links
```

Internal engineering notes belong in `../docs/`, not here.
