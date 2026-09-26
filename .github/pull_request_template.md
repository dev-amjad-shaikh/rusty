## Summary

<!-- What does this change do, and why? Link the issue it closes, if any. -->

## Components touched

<!-- Check every component this PR modifies. -->

- [ ] `rusty-core` (`rusty-agent-runtime`)
- [ ] `rusty-server` (`rusty-agent-server`)
- [ ] `rusty-worker`
- [ ] `rusty-otel`
- [ ] `studio/`
- [ ] Python SDK (`sdks/python`)
- [ ] TypeScript SDK (`sdks/typescript`)
- [ ] Docs / repo tooling only

## Test evidence

<!-- Paste the checks you ran and their results. For Rust crates, the expected trio is:
     cargo fmt --all -- --check
     cargo clippy --all-targets -- -D warnings
     cargo test
     run from the directory of each crate you touched. For SDK changes, include the
     relevant e2e suite (it boots the real server_demo binary). -->

```
```

## Breaking changes

- [ ] This PR contains a breaking change (public API, wire protocol, on-disk store layout, or behavior change that existing users must adapt to).

<!-- If checked, describe the breakage and the migration path below, and make sure
     CHANGELOG.md records it under the component's next release. -->
