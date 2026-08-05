## What this changes

<!-- What the change does, and why it is needed. -->

## How it was verified

<!-- Which of these you ran, and anything you checked by hand. -->

- [ ] `cargo fmt --all` and `cargo clippy --all-targets --all-features` are clean
- [ ] `cargo test --all-features` passes, including a test that fails without this change
- [ ] `mkdocs build --strict` (if `docs/` changed)
- [ ] `scripts/verify-with-dotnet.sh` (if anything a NuGet client observes changed:
      service index, flat container, registration, search, download URLs, symbol keys)

## Notes

<!-- Anything reviewers should know: trade-offs, follow-ups, config changes. -->
