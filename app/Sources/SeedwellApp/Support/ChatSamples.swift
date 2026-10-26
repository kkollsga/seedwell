import Foundation

/// Complex chat messages, the way agents and people write them: the snapshot renders them (`chat-markdown-*`) and the
/// tests parse them, so the Markdown of SPEC 7G.7 is checked on real shapes rather than one-liners.
enum ChatSamples {
    /// (agent label or nil, the computer, the body).
    static let messages: [(agent: String?, computer: String, body: String)] = [
        ("Seedwell", "Kristian’s Mac mini", report),
        ("Website", "Studio iMac", table),
        (nil, "Kari's iPhone", "Takk! 🙌 Kan dere sjekke **lørdag** før ~~fredag~~? Se [planen](https://example.com/plan) og `~/Shared/Middag.pdf`."),
        ("Seedwell", "Kristian’s MacBook Air", code),
        ("ci-bot", "Studio iMac", nested),
        (nil, "Studio iMac", "Ok"),
        ("mcp-servers", "Kristian’s Mac mini", bugMail),
        ("kglite", "Studio iMac", statusMail),
    ]

    static let bugMail = """
    ---
    mail: 1
    to: kglite
    type: bug
    summary: The open-source graph serves the wrong repo after a switch
    ---
    After `repo_management('pydata/xarray')`, `graph_overview` still answers for the previous repo.

    ## Ask
    1. Rebuild the active graph when the repo changes.
    2. Tell us the version with the fix.

    ## References
    - `mcp-servers/open_source/server.py:212`
    """

    static let statusMail = """
    ---
    mail: 1
    to: mcp-servers
    type: status
    re: 7f1b588c
    state: done
    ---
    Fixed in **0.17.13**: the graph is rebuilt on every switch. Plan in `dev-docs/plans/open-source.md`; nothing pending.
    """

    static let report = """
    # Build report for 0.1.7

    All **283** Rust tests and *292* Swift tests passed.
    Details are in `target/nextest/report.xml`.

    ## Steps

    1. Bumped the version in `Cargo.toml`
    2. Ran the checks:
       - `tools/check.sh` (clippy and the tests)
       - the Xcode build with Swift 6.3.3
    3. Built the public commit

    ### Next

    - [x] CI green
    - [ ] Tag `v0.1.7`
    - [ ] Publish the draft

    > The daylight saving change today moved the times by one hour.
    > **Check** the stamps in older logs.

    ---

    ```rust
    fn time(at: u64) -> String {
        clock(at, offset_at(at)) // local, with the offset of that moment
    }
    ```
    """

    static let table = """
    ## Download page

    | Build | Size | SHA-256 | Notes |
    |:------|-----:|---------|-------|
    | 0.1.6 | 13.4 MB | `113c97b5…` | published |
    | 0.1.7 | 13.6 MB | `pending` | **draft** |
    | 0.1.5 | 12.9 MB | | old \\| kept |

    The table is in `site/download.md`.
    """

    static let code = """
    The share extension logs one line per use. To see them:

    ~~~sh
    /usr/bin/log show --last 10m --info --style compact \\
      --predicate 'subsystem == "io.github.kkollsga.Seedwell" AND category == "share-menu"'
    # expect: share: not synced | share dialog | may not share
    ```not a fence end```
    ~~~

    And a long line without a language, which scrolls sideways instead of wrapping:

    ```
    2026-10-25 20:14:03.118 SeedwellShare[76855:1203442] share: not synced for <private> (pid 76855, build 143, the 15:15 release candidate before the published 0.1.6)
    ```
    """

    static let nested = """
    7. Seventh, numbered as written
    8. Eighth
       continued on an indented line
       - a point under it
         - and one deeper
           - and the deepest
    9. Ninth with `code`, **bold** and a [link](https://example.com)

    #hashtag, 3.5 apples and a lone 1. are plain text.
    """
}
