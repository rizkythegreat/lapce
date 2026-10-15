# Custom macOS build (forks)

`.github/workflows/custom-macos.yml` builds an Apple Silicon `Lapce Split.app`
from a fork, without Apple signing or notarization secrets. It is meant for
personal use of a fork's changes and does not run in `lapce/lapce`.

## Getting the app

1. Make sure GitHub Actions is enabled for the fork (Actions tab, then
   "I understand my workflows, go ahead and enable them").
2. Push to `feat/split-terminal`, or open
   Actions → "Custom macOS build" → "Run workflow" (that button only appears
   once the workflow file is on the default branch). Choose `release-lto` for
   a smaller, faster binary at the cost of a much longer build.
3. When the run finishes, download the `lapce-split-macos-arm64` artifact from
   the run's summary page. It contains `Lapce-Split-macos-arm64-<sha>.zip`.
4. Unzip it and move `Lapce Split.app` to `/Applications`.

## Living next to the official Lapce

- The app is named `Lapce Split.app` and uses its own bundle identifier,
  `io.github.<owner>.lapce-split`, so it does not replace `Lapce.app`.
- Builds that are not made from a `v*` tag are "Nightly" builds, so settings,
  keymaps and plugins live in a `Lapce-Nightly` data directory, separate
  from `Lapce-Stable`. If you also run the official Nightly, they share it.
- The self-updater is compiled out, so the app never replaces itself with an
  upstream release.

## Gatekeeper

The app is ad-hoc signed (`codesign --sign -`): it has a valid signature
with no developer identity and is not notarized, so macOS blocks the first
launch with a message that Apple could not verify it. To open it anyway:

1. Try to open `Lapce Split.app` once and dismiss the dialog.
2. Open System Settings → Privacy & Security, scroll to the message about
   "Lapce Split" and click "Open Anyway", then confirm.

This approval is per build: a newly downloaded build asks again. Removing the
quarantine attribute (`xattr -dr com.apple.quarantine "/Applications/Lapce Split.app"`)
has the same effect; only do that for builds you made from your own fork.

Gatekeeper is not disabled globally, and no signing certificates or secrets
are used. A properly signed and notarized build needs an Apple Developer ID
and the `MACOS_CERTIFICATE*` / `NOTARIZE_*` secrets used by `release.yml`.

## Building locally instead

On an Apple Silicon Mac with Rust installed:

```sh
cargo build --profile release --bin lapce
```

Then follow the "Bundle and ad-hoc sign" step of the workflow. Expect a long
first build; machines with 8 GB of RAM can build Lapce but should avoid
running other heavy applications during the build.
