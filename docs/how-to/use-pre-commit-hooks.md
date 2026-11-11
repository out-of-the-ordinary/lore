# Use pre-commit hooks

Run your own policy checks, such as "the message names a ticket" or "every Unity asset travels with its `.meta`", before `lore commit` and `lore revision amend` record a revision. This guide assumes a working tree you commit from with the `lore` CLI, which includes tools that shell out to `lore.exe`.

## Prerequisites

- A `lore` build that has the `--no-verify` flag on `lore commit`.
- A script interpreter for your hook: Windows PowerShell 5.1 or `pwsh` on Windows, `sh` and `jq` on macOS and Linux.

## Steps

1. **Declare the hook in `.lorehooks.toml` at the repository root.** Commit the file, so the whole team shares it.

   ```toml
   version = 1

   [pre_commit]
   command         = ["sh", "tools/lore-hooks/pre-commit.sh"]
   command_windows = ["powershell.exe", "-NoLogo", "-NoProfile", "-NonInteractive",
                      "-ExecutionPolicy", "Bypass", "-File", "tools/lore-hooks/pre-commit.ps1"]
   timeout_secs    = 30
   ```

   The command is an argument list, with no shell involved. A first element with a `/` or `\` is relative to the repository root, and a bare name is looked up on `PATH`. The hook runs with the repository root as its working directory. `command_windows` and `command_unix` override `command` on that platform. `timeout_secs` defaults to 60 and accepts 1 to 600. Unknown keys and any `version` other than `1` are errors, and an error aborts the commit.

2. **Trust the repository.** A hook is code from the repository, so Lore runs it only when your user-level [`cli.toml`](../reference/lore-cli-config.md#hooks-table) trusts the repository's remote or path.

   ```toml
   [hooks]
   trusted = ["lore://lore.int.example.com"]
   ```

   A repository that has a `.lorehooks.toml` but is not trusted prints a warning with the exact lines to add, and the commit goes ahead without the hook. Set `LORE_TRUST_HOOKS=1` to trust everything for one process, which is meant for CI.

3. **Write the hook.** Lore passes the staged change as JSON. Read it from the file named by `LORE_HOOK_PAYLOAD`. Exit 0 to let the commit proceed and any other code to abort it. Everything the hook prints reaches your terminal.

4. **Commit as usual.** A rejected commit exits with code 100 and records nothing. Skip the hook once with `--no-verify`, or for a process with `LORE_NO_VERIFY=1`.

   ```bash
   lore commit "LQ-123 Fix hero idle animation"
   lore commit --no-verify "WIP"
   ```

## The payload

The hook receives UTF-8 JSON without a byte order mark, as a temporary file whose path is in `LORE_HOOK_PAYLOAD`. The same bytes arrive on stdin. **On Windows, read the file**: Windows PowerShell decodes stdin with the console's OEM code page, which mangles non-ASCII commit messages.

```json
{
  "version": 1,
  "hook": "pre-commit",
  "operation": "commit",
  "repository_root": "C:\Work\MyGame",
  "repository_id": "01a1c7efffad76e3bfbc975fe6e8d17a",
  "remote_url": "lore://lore.int.example.com/my-game",
  "branch": "main",
  "revision": "<current revision hash>",
  "revision_number": 41,
  "merge": false,
  "dry_run": false,
  "force": false,
  "interactive": true,
  "message": "LQ-123 Fix hero idle anim\n\nlonger body",
  "link": null,
  "layer": null,
  "link_messages": {},
  "layer_messages": {},
  "files": [
    { "path": "Assets/Hero.png",       "action": "add",  "type": "file", "from_path": null, "merged": false },
    { "path": "Assets/New/Old.prefab", "action": "move", "type": "file", "from_path": "Assets/Old.prefab", "merged": false }
  ]
}
```

- `operation` is `commit` or `amend`. An amend only rewrites the message, so its `files` list is empty.
- `action` is `add`, `delete`, `move`, `copy` or `modify`. `type` is `file`, `directory` or `link`. `from_path` is set for moves and copies. Paths are relative to the repository root and use `/`.
- `merge` is true when the commit completes a merge. `files` is sorted by path and lists staged entries without unresolved conflicts.
- `dry_run` is true under `--dry-run`: the hook still runs, so you can test a policy without committing. A hook must not modify the repository.

These variables are set for the hook, on top of your own environment: `LORE_HOOK` (`pre-commit`), `LORE_HOOK_PAYLOAD`, `LORE_HOOK_OPERATION`, `LORE_REPOSITORY_ROOT`, `LORE_BRANCH`, `LORE_HOOK_INTERACTIVE` (`1` or `0`), `LORE_HOOK_JSON` and `LORE_HOOK_DRY_RUN`. A hook that runs `lore commit` itself does not recurse, because `LORE_HOOK` is already set.

With `--json`, the hook's stdout goes to Lore's stderr so that stdout stays parseable, and a rejection prints `{"error":"pre-commit hook rejected the commit","hook":"pre-commit","exit_code":N}` on stderr.

## Example policy

The scripts below check that the message names a Linear ticket and that every added, deleted or moved Unity asset travels with its `.meta`. They live in your project repository, as `tools/lore-hooks/pre-commit.ps1` and `tools/lore-hooks/pre-commit.sh`.

- `LORE_POLICY_MODE=warn|block` chooses between printing problems and aborting. The default is `warn`.
- The team prefixes sit at the top of each script, and `LORE_POLICY_PREFIXES` overrides them (comma separated for PowerShell, `|` separated for `sh`).
- When `LINEAR_API_KEY` (a personal API key) is set, the script also checks that the ticket exists. It only warns when Linear is unreachable.

### `tools/lore-hooks/pre-commit.ps1`

```powershell
# Lore pre-commit policy: Linear ticket ID + Unity .meta pairing. Windows PowerShell 5.1 compatible.
$ErrorActionPreference = 'Stop'
$Mode         = if ($env:LORE_POLICY_MODE) { $env:LORE_POLICY_MODE } else { 'warn' }  # warn | block
$TeamPrefixes = @('LQ', 'SCR', 'SYSTEM')           # Linear team keys accepted in messages
if ($env:LORE_POLICY_PREFIXES) { $TeamPrefixes = $env:LORE_POLICY_PREFIXES -split '[,\s]+' | Where-Object { $_ } }
$MetaRoots    = @('Assets/', 'Packages/')          # folders whose entries need a .meta

# Read the payload file, not stdin: stdin is decoded with the OEM codepage on Windows.
$payload  = Get-Content -LiteralPath $env:LORE_HOOK_PAYLOAD -Raw -Encoding UTF8 | ConvertFrom-Json
$problems = New-Object System.Collections.Generic.List[string]

# 1. Linear ticket ID in the message (and optionally: does it exist?)
$pattern = '\b(' + (($TeamPrefixes | ForEach-Object { [regex]::Escape($_) }) -join '|') + ')-\d+\b'
$m = [regex]::Match([string]$payload.message, $pattern)
if (-not $m.Success) {
    $problems.Add("message has no Linear ticket ID (expected e.g. $($TeamPrefixes[0])-123)")
} elseif ($env:LINEAR_API_KEY) {
    try {
        $body = @{ query = 'query($id:String!){ issue(id:$id){ identifier } }'; variables = @{ id = $m.Value } } |
                ConvertTo-Json -Compress
        $r = Invoke-RestMethod -Uri 'https://api.linear.app/graphql' -Method Post -TimeoutSec 5 `
             -Headers @{ Authorization = $env:LINEAR_API_KEY } -ContentType 'application/json' -Body $body
        if (-not $r.data.issue) { $problems.Add("Linear issue $($m.Value) does not exist") }
    } catch {
        [Console]::Error.WriteLine("lore pre-commit: Linear unreachable ($($_.Exception.Message)); existence not checked")
    }
}

# 2. Unity .meta pairing: an added/deleted/moved asset and its .meta must travel together
$byPath = @{}
foreach ($f in $payload.files) { $byPath[$f.path] = $f }
foreach ($f in $payload.files) {
    if (@('add', 'delete', 'move') -notcontains $f.action) { continue }
    $p = [string]$f.path
    if (-not ($MetaRoots | Where-Object { $p.StartsWith($_) })) { continue }
    if ($p.EndsWith('.meta')) { $partner = $p.Substring(0, $p.Length - 5) } else { $partner = "$p.meta" }
    # a folder's .meta pairs with a directory, which may not be listed as a staged entry
    if ($p.EndsWith('.meta') -and (Test-Path -LiteralPath (Join-Path $env:LORE_REPOSITORY_ROOT $partner) -PathType Container)) { continue }
    $other = $byPath[$partner]
    if (-not $other -or $other.action -ne $f.action) {
        $problems.Add("$($f.action) $p without a matching $($f.action) of $partner")
    }
}

if ($problems.Count -eq 0) { exit 0 }
foreach ($p in $problems) { [Console]::Error.WriteLine("lore pre-commit: $p") }
if ($Mode -eq 'block') {
    [Console]::Error.WriteLine('Commit blocked. Fix the above, or bypass with: lore commit --no-verify ...')
    exit 1
}
[Console]::Error.WriteLine('lore pre-commit: warnings only, commit continues')
exit 0
```

### `tools/lore-hooks/pre-commit.sh`

```sh
#!/bin/sh
# Lore pre-commit policy: Linear ticket ID + Unity .meta pairing. Needs jq (and curl for the Linear check).
set -eu
MODE="${LORE_POLICY_MODE:-warn}"            # warn | block
TEAM_PREFIXES="${LORE_POLICY_PREFIXES:-LQ|SCR|SYSTEM}"   # Linear team keys, '|' separated
META_ROOTS='^(Assets|Packages)/'            # folders whose entries need a .meta
P="$LORE_HOOK_PAYLOAD"
problems=''
add() { problems="${problems}lore pre-commit: $1
"; }

# 1. Linear ticket ID
ticket=$(jq -r '.message' "$P" | grep -Eo "\b($TEAM_PREFIXES)-[0-9]+\b" | head -n 1 || true)
if [ -z "$ticket" ]; then
  add "message has no Linear ticket ID (expected e.g. LQ-123)"
elif [ -n "${LINEAR_API_KEY:-}" ]; then
  body=$(jq -nc --arg id "$ticket" '{query:"query($id:String!){issue(id:$id){identifier}}",variables:{id:$id}}')
  if resp=$(curl -fsS -m 5 -H "Authorization: $LINEAR_API_KEY" -H 'Content-Type: application/json' \
              -d "$body" https://api.linear.app/graphql 2>/dev/null); then
    if [ -z "$(printf '%s' "$resp" | jq -r '.data.issue.identifier // empty')" ]; then
      add "Linear issue $ticket does not exist"
    fi
  else
    echo "lore pre-commit: Linear unreachable; existence not checked" >&2
  fi
fi

# 2. Unity .meta pairing
TAB=$(printf '\t')
jq -r --arg re "$META_ROOTS" '
  (.files | map({key: .path, value: .action}) | from_entries) as $by
  | .files[]
  | select(.action == "add" or .action == "delete" or .action == "move")
  | select(.path | test($re))
  | .path as $p
  | (if ($p | endswith(".meta")) then $p[0:-5] else $p + ".meta" end) as $q
  | select($by[$q] != .action)
  | "\(.action)\t\($p)\t\($q)"' "$P" > "${P}.pairs"
while IFS="$TAB" read -r action path partner; do
  case "$path" in *.meta) [ -d "$LORE_REPOSITORY_ROOT/$partner" ] && continue ;; esac
  add "$action $path without a matching $action of $partner"
done < "${P}.pairs"
rm -f "${P}.pairs"

[ -z "$problems" ] && exit 0
printf '%s' "$problems" >&2
if [ "$MODE" = block ]; then
  echo 'Commit blocked. Fix the above, or bypass with: lore commit --no-verify ...' >&2
  exit 1
fi
echo 'lore pre-commit: warnings only, commit continues' >&2
exit 0
```

## What is not covered

- **Auto-commits are not hooked.** Revisions that Lore creates itself, namely `lore branch merge` with automatic commit, `lore revision cherry-pick`, `lore revision revert` and `lore revision restore`, do not run the hook: their messages are generated and their content comes from existing revisions. A merge that stops on conflicts ends with a plain `lore commit`, which does run the hook, with `"merge": true`.
- **Programs that link Lore directly** (the library or the C API) do not run it. Only the `lore` CLI does.
- **The hook checks the staged set, not file content.** A script that reads file content reads the working tree, which can differ from what was staged. Path and action checks, like the ones above, are exact.
- **Client hooks are advisory.** Anyone can pass `--no-verify`, and an untrusted repository skips the hook. Treat the hook as fast feedback and enforce hard rules on the server.
- **Timeouts kill only the direct child on Windows.** A hook that starts other processes can leave them running after a timeout.

## Result

`lore commit` runs your policy before it records anything. A rejection exits with code 100 and leaves the staged changes in place.

## See also

- [Lore CLI configuration reference](../reference/lore-cli-config.md) — `.lorehooks.toml`, the `[hooks]` table of `cli.toml` and the hook environment variables.
- [Lore CLI command reference](../reference/lore-cli-commands.md) — `--no-verify` on `lore commit`, `lore revision commit` and `lore revision amend`.
