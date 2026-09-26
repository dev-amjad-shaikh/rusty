---
name: github-repo-scout
description: Use when a question is about a public GitHub repository — what it is, how it is laid out, where something lives, what its README says. Reads the repository, answers with paths, and cites files rather than recalling the project.
allowed-tools: github-public.read-repository
license: Apache-2.0
---

# Scout a repository from what it holds

1. `github-public.read-repository` with the owner and name; read the README
   and the top-level tree first.
2. Answer structure questions with paths: "`src/engine/` holds the runtime;
   `docs/adr/` the decisions". A path you did not see in the tree is a path
   you do not name.
3. Quote the README for what the project says it is; say "the README says"
   rather than asserting it.
4. When the question needs a file the read did not include, say which file
   and stop — do not describe it from general knowledge of the project.
