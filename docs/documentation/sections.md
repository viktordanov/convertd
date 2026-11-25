# Memoria section IDs

A section marker ties part of a README to the source files that it explains. When one of those files changes, the marker shows the reviewer where to start reading. It is a hint only: every review still covers the whole document.

## The shared IDs

Every README picks from one list, so that the same concern has the same name across the repository:

| ID | The part of the README that explains… |
| --- | --- |
| `overview` | what the boundary is and how it fits into the service |
| `setup` | what the reader needs before use: requirements, settings, start-up |
| `usage` | the operations, with their inputs and their results |
| `contract` | what the boundary guarantees, its limits, and what a caller must do |
| `lifecycle` | start, recycling, shutdown and clean-up |
| `failure` | errors, timeouts and crashes, and what the reader sees for each |
| `measurement` | what is measured, the method, and the current numbers |

A README uses only the IDs it needs. When one topic needs its own start point, give it a specific ID such as `pool` or `job-directory`. Do not add a second name for a concern that the table already has. An ID stays the same when its heading changes.

## A marker

Write the prose first. Then wrap it and name the files that support it:

```markdown
<!-- memoria:section id="lifecycle" files="pool.rs process.rs" -->
## Recycling

The pool replaces a worker after its job limit, its memory limit or its age.
<!-- /memoria:section -->
```

The section starts with its heading. Paths are relative to the README and name files in its own scope. A path cannot point into a folder that the README hands to another README, and it cannot name another tracked document. Name files one by one: globs are not allowed. Map a file only when the section explains it.
