# ubra in homebrew/cask

Today ubra installs from its own tap:

```sh
brew install --cask Ubra-Dev/ubra-app/ubra
```

This directory prepares the move to Homebrew's official cask repository, so
that `brew install --cask ubra` works with no tap:

- `ubra.rb` is the candidate cask. Version and checksum are rendered from the
  latest GitHub release at submission time, so the values committed here only
  have to be well formed.
- `submit-to-homebrew-cask.sh` renders it, places it in a local
  `homebrew/cask` checkout, runs Homebrew's checks, commits it, and with
  `--open-pr` opens the pull request.
- `.github/workflows/homebrew-cask.yml` runs `brew style`,
  `brew audit --new --online` and `brew livecheck` on the rendered candidate on
  a clean macOS runner whenever this directory changes.

Policy references below are to Homebrew's docs as shipped with Homebrew 7.0.6
(`Acceptable-Casks.md` reviewed 2026-08-25, `Package-Acceptance-Policy.md`
2026-07-18, `Responsible-AI-Usage.md` 2026-07-26). Re-read them before
submitting; they change.

## Acceptance checklist

| Requirement | Status | Evidence (2026-10-01) |
| --- | --- | --- |
| Notability: a self-submission by the repository owner needs 90 forks, 90 watchers **or 225 stars** | Met | 338 stars, 25 forks (`gh api repos/Ubra-Dev/ubra-app`) |
| Repository at least 30 days old | Met | created 2026-08-04 |
| Public presence and a homepage that explains the project | Met | https://getubra.com (also the repository's homepage field) |
| Actively maintained | Met | Active development on 0.1.0; no public releases yet |
| Download published by the developer, immutable and versioned | Met | GitHub release asset `ubra-<version>-universal.dmg`; release assets are never replaced (`UPDATING.md`) |
| Works on every declared OS and architecture, including the latest macOS | Met | Universal (arm64 + x86_64) DMG; `LSMinimumSystemVersion` 15.0; built and run on macOS 27 |
| Passes Gatekeeper | Met | `spctl -a -vv ubra.app`: `accepted`, `source=Notarized Developer ID`; ticket stapled |
| Token is free and follows the token reference | Met | No `ubra` in `formulae.brew.sh/api/cask.json`; app bundle is `ubra.app`, so the token is `ubra` |
| Not previously refused | Met | No closed `ubra ... (new cask)` pull request in Homebrew/homebrew-cask |
| Correct package type | Met | Graphical app shipped as a compiled, notarized build; casks are the right type |
| `desc` describes what it does, no slogan, no name, no platform | Met | `Terminal workspace for running coding agents in parallel` (the tap's "Best way to work with coding agents" is a slogan and would be rejected) |
| `depends_on macos:` matches the bundle | Met | `:sequoia` = 15.0 = `LSMinimumSystemVersion`; `brew audit --online` compares the two |
| `auto_updates true` only for apps that download and install their own updates | Met | ubra's updater downloads, verifies and installs releases itself (`UPDATING.md`) |
| `livecheck` | Met | `url :url` + `strategy :github_latest`; no `extract_plist`, so the cask stays on the autobump list |
| `zap` | Met, with one documented omission | See below |

### Things the old tap cask does that Homebrew 7 rejects or warns about

- `desc` was a slogan.
- `homepage` was the GitHub repository; the product site is https://getubra.com.
- The `verified:` URL parameter is deprecated in Homebrew 7 (`brew info`
  prints a deprecation warning), so the candidate does not use it even though
  the download host differs from the homepage.

## The `zap` decision

`brew uninstall --zap --cask ubra` runs only when a user asks for it, and
Homebrew's guidance is that `zap` removes preferences and caches under
`~/Library` but not files the user created.

ubra keeps two kinds of data:

- **App data** (`com.ubra.ubra` bundle id): preferences, window state,
  `~/Library/Application Support/ubra` (UI prefs, notification history,
  quick-open and usage caches), `~/Library/Caches/ubra` (update downloads),
  and the WebKit stores of the built-in browser. All of these are zapped.
- **Engine data** in `~/Library/Application Support/Ubra`: the session
  registry, the Unix socket and holder state of agent sessions that keep
  running after the app quits, remote host definitions, saved agent account
  logins, task and message databases, and the user's notes. This is **not**
  zapped.

Why the Engine directory stays out:

- It contains notes the user wrote, which Homebrew says `zap` should not remove.
- Agent sessions outlive the app on purpose. Zapping while they run would
  delete the sockets and records their holders depend on, leaving live agent
  processes the app can no longer find, attach to, or stop.
- It contains saved agent logins; deleting them silently is a worse surprise
  than leaving them.

The cask says this in a three-line comment above `zap`, which is the form
Homebrew reviewers expect for a deliberate omission. Users who want everything
gone can quit ubra, stop its sessions, and delete
`~/Library/Application Support/Ubra` themselves.

If a reviewer insists on including it: add
`"~/Library/Application Support/Ubra"` to the `zap trash:` array. `trash:`
moves to the Trash rather than deleting, so it is recoverable. Do not argue the
point; Homebrew's contributing guide asks submitters to implement what
maintainers request.

## Submitting

Homebrew requires all of the following from the person whose name is on the
pull request:

- review the cask yourself, including every `zap` path;
- disclose AI assistance in the PR body (the script writes this disclosure);
- no AI trailers in the commit (the script's commit message has none);
- answer review comments yourself, without AI;
- non-maintainers may have only one AI-assisted PR open at a time.

Steps:

1. Make sure `brew audit` can run on this Mac. On macOS 27 with Command Line
   Tools 26.x, every `brew audit` stops with "Your Command Line Tools are too
   outdated" before it looks at the cask. Install the Command Line Tools for
   Xcode 27 (Software Update, or developer.apple.com/download/all).
2. Prepare, without submitting:

   ```sh
   packaging/homebrew/submit-to-homebrew-cask.sh
   ```

   It taps `homebrew/cask` with `--force` if needed (a full clone), creates the
   branch `ubra-new-cask` from `origin/HEAD`, writes `Casks/d/ubra.rb` for the
   latest release (checksum from GitHub's asset digest), runs
   `brew style --fix --cask` and
   `HOMEBREW_NO_INSTALL_FROM_API=1 brew audit --new --cask --online homebrew/cask/ubra`,
   and commits `ubra <version> (new cask)`. It refuses if the cask already
   exists upstream (use `brew bump-cask-pr` then) or if a closed
   `ubra ... (new cask)` PR exists.
3. In that checkout (`cd "$(brew --repository homebrew/cask)"`), run what the
   PR template asks for. Installing replaces the installed ubra.app, so do it
   when that is acceptable:

   ```sh
   HOMEBREW_NO_INSTALL_FROM_API=1 brew install --cask ubra
   brew uninstall --cask ubra
   brew lgtm --online
   ```

4. Submit:

   ```sh
   packaging/homebrew/submit-to-homebrew-cask.sh --reviewed --install-tested --open-pr
   ```

   This forks Homebrew/homebrew-cask under your account if you have no fork,
   pushes `ubra-new-cask` and opens `ubra <version> (new cask)` with the PR
   template filled in. Without `--install-tested` the install and uninstall
   boxes stay unticked.

`brew bump-cask-pr` does not apply here: it updates casks that already exist
in `homebrew/cask`.

## The release flow after the cask is merged

- **Homebrew bumps the cask.** New casks in `homebrew/cask` are on the
  autobump list by default (`docs/Autobump.md`). BrewTestBot runs livecheck
  every 3 hours and opens the version bump PR itself, so a release reaches
  `brew install --cask ubra` within hours without anything from this repo.
  The release must stay marked "Latest" on GitHub, because livecheck uses
  `github_latest`, and the asset must keep the name
  `ubra-<version>-universal.dmg`.
- **Do not bump it by hand.** Homebrew asks contributors not to open manual
  bumps for autobumped packages.
- **`auto_updates true` still applies.** `brew upgrade` leaves ubra alone
  unless run with `--greedy`, and the app keeps updating itself.
- **`release.sh` step 6** keeps pushing the custom tap until the tap is
  retired. After that, run releases with `SKIP_CASK=1` and remove step 6 and
  `publish-homebrew-cask.sh` in a follow-up.

## Retiring the custom tap

After the official cask is merged and https://formulae.brew.sh/cask/ubra
exists, make one commit in `Ubra-Dev/homebrew-ubra` that:

1. deletes `Casks/ubra.rb`, and
2. adds `tap_migrations.json` at the tap root:

   ```json
   {
     "ubra": "homebrew/cask"
   }
   ```

Homebrew reads a tap's `tap_migrations.json` for casks as well as formulae.
When the cask is gone from the tap, the cask loader resolves
`Ubra-Dev/ubra-app/ubra` to `homebrew/cask/ubra`
(`Cask::CaskLoader.tap_cask_token_type` in Homebrew 7.0.6). Existing installs
keep working because the app updates itself. Keeping a second `ubra` cask in
the tap would only make the short name ambiguous for users who still have the
tap.

Check this on a Mac that has the tap: `brew update`, then
`brew info --cask Ubra-Dev/ubra-app/ubra` should point at `homebrew/cask`.
Users can then run `brew untap Ubra-Dev/ubra-app`.

Also update the install instructions to `brew install --cask ubra`:
`README.md` (which still says the project is below the
notability threshold), the website, and the tap's own README.
