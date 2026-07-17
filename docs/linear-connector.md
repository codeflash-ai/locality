# Linear Connector

The Linear connector is a first-party Locality source connector named `linear`.
It uses Linear's GraphQL API for issue metadata, issue body fetch/render, and
approved issue updates.

The initial projection groups issues by team:

```text
Engineering/
  ENG-1/
    page.md
```

Team directories are metadata containers. Issue `page.md` files are canonical
Markdown with Linear reference fields in frontmatter and the Linear issue
description as the body. Rendered references use the `Label <id>` shape so local
diff can ignore label-only refreshes while preserving the stable Linear UUID.

The connector currently supports:

- full issue enumeration;
- lazy root/team child listing;
- single-issue observation and batch observation;
- issue fetch/render;
- whole-entity body updates mapped to Linear issue descriptions;
- property updates for title, status, project, and assignee.

Unsupported properties fail closed before remote mutation. Undo and create/move
operations remain future work for the native connector.
