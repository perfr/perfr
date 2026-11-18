# Security policy

## Reporting a vulnerability

Report it privately through GitHub: **[Report a vulnerability](https://github.com/perfr/perfr/security/advisories/new)**
(the repository's Security tab, then Advisories). Please do not open a
public issue or pull request for something that could be used against a
perfr user before a fix is out.

Say what you found, the version or commit, the platform, and how to
reproduce it. A capture or trace file that triggers it is the most useful
thing you can attach.

What to expect:

- An answer within 7 days saying whether we can reproduce it.
- A fix or a plan within 30 days for anything confirmed. If it will take
  longer we will say so and why.
- Disclosure together: the advisory is published when a release with the fix
  is out, or 90 days after the report, whichever comes first, and it credits
  you unless you ask otherwise.

## Supported versions

The latest release, and `main`. perfr is pre-1.0 and fixes are not
back-ported: the fix is a new release, which `perfr update` installs.

## What counts

perfr attaches to processes, opens files from other people and runs a local
web server, so these are in scope:

- **Opening a file.** A crafted `.perfr.zip`, `.perfr.stream`, Chrome or
  Perfetto trace, or a binary being symbolized that crashes the
  service or the viewer, reads or writes memory it should not, or runs code.
- **The local service.** A web page on another origin, or another account on
  the machine, reaching the HTTP API or the WebSocket. The service has no
  login; it decides by `Host` and `Origin` and, on Linux, by who the peer
  is. A way around those checks is a vulnerability.
- **Other machines.** Anything a joined remote or a hub can do to the other
  side beyond sending it a capture.
- **Privilege.** Anything that lets a user get more than their own rights
  through perfr's setup (`perfr setup`, the sudo wrapper, the capabilities it
  asks for).
- **Getting perfr.** The install script, `perfr update`, the release
  artifacts and the wheels: anything that would have a user run something
  other than what was released.
- **The viewer in a page.** A capture or a share link that makes the viewer
  run script, open an address its author chose, or read another capture.

These are how perfr is meant to work, and are not vulnerabilities by
themselves:

- `--host 0.0.0.0` puts the API on the network with no authentication, as
  its help and the manual say. Whoever can reach the port can attach to
  processes and start captures.
- A capture is a record of the machine it was made on: process and thread
  names, function names, paths of binaries, the hostname, the CPU. Sharing
  a capture shares that.
- A profiler reads the memory and the call stacks of what it profiles, with
  the permissions the system grants it.

The share service and the website (perfr.dev) are a separate codebase.
Report issues with them the same way, here.

## What the repository does about it

- Everything the workflows run is pinned to a commit or a hash, and
  Dependabot proposes the updates.
- CodeQL scans the tree twice a week and OpenSSF Scorecard once.
- The parsers of untrusted input are fuzzed weekly ([`fuzz/`](fuzz/README.md)).
- Advisories against the dependencies are fixed by updating. Those with no
  fix are listed, each with its reason and an expiry, in `osv-scanner.toml`.
