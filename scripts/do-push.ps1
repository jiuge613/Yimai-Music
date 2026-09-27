# 把工作区推送到 jiuge613/YimaiMusic 并触发 Release 流水线。
# 沿用 WorkBuddy _push/push.py 已验证可行的方式：便携 git + 独立克隆目录。
# 令牌只存在于内存变量与临时 remote url 中，推送后立即把 remote 还原为干净地址。
$ErrorActionPreference = 'Stop'
$GIT   = 'D:\mingit\cmd\git.exe'
$SRC   = 'G:\Yimai Music\YimaiMusic-main'
$WORK  = Join-Path $SRC '_release_repo'
$TOK   = (Get-Content 'C:\Users\Administrator\WorkBuddy\2026-09-23-22-54-41\_push\token.txt' -Raw).Trim()
$OWNER = 'jiuge613'; $REPO = 'YimaiMusic'; $BRANCH = 'main'
$CLEAN_REMOTE = "https://github.com/$OWNER/$REPO.git"

if (-not (Test-Path $GIT)) { Write-Output "ERR: git not found"; exit 1 }

# 需要排除的大目录（避免拷贝 node_modules / target 这类几百 MB 的东西）
$SKIP_DIRS = @('node_modules','target','dist','.mimosa','.zcode','.git','_release_repo','_push','output','gen')

function Copy-Tree([string]$s, [string]$d) {
  if (-not (Test-Path $d)) { New-Item -ItemType Directory -Path $d -Force | Out-Null }
  foreach ($item in Get-ChildItem -LiteralPath $s -Force) {
    if ($item.PSIsContainer) {
      if ($SKIP_DIRS -contains $item.Name) { continue }
      Copy-Tree $item.FullName (Join-Path $d $item.Name)
    } else {
      Copy-Item -LiteralPath $item.FullName -Destination (Join-Path $d $item.Name) -Force
    }
  }
}

Write-Output "=== 1) fetch remote main ==="
if (Test-Path (Join-Path $WORK '.git')) {
  & $GIT -C $WORK fetch origin $BRANCH --depth=1 2>&1 | Out-Null
  if ($LASTEXITCODE -ne 0) { Write-Output "fetch failed, retrying..."; Start-Sleep 5; & $GIT -C $WORK fetch origin $BRANCH --depth=1 }
  & $GIT -C $WORK reset --hard FETCH_HEAD
} else {
  New-Item -ItemType Directory -Path $WORK -Force | Out-Null
  & $GIT -C $WORK init -b $BRANCH
  & $GIT -C $WORK config user.email "yimai-bot@users.noreply.github.com"
  & $GIT -C $WORK config user.name  "Yimai Bot"
  & $GIT -C $WORK config core.autocrlf false
  & $GIT -C $WORK remote add origin "https://$TOK@github.com/$OWNER/$REPO.git"
  & $GIT -C $WORK fetch origin $BRANCH --depth=1
  & $GIT -C $WORK reset --hard FETCH_HEAD
}

Write-Output "=== 2) overlay workspace files ==="
Copy-Tree $SRC $WORK
Write-Output "  overlay done"

Write-Output "=== 3) stage & review ==="
& $GIT -C $WORK add -A
$st = & $GIT -C $WORK status --porcelain
$changed = @($st | Where-Object { $_ -notmatch '^\?\?' }).Count
$untracked = @($st | Where-Object { $_ -match '^\?\?' }).Count
Write-Output "  modified/staged: $changed   untracked: $untracked"
Write-Output "  --- first 40 changed paths ---"
$st | Select-Object -First 40 | ForEach-Object { Write-Output ("    " + $_) }

Write-Output "=== 4) commit & push ==="
if ($changed -gt 0) {
  $msg = "feat: v1.0.0 WASAPI exclusive mode, 5-tier quality, lyric fallback, UI contrast fixes"
  $body = @"
- WASAPI exclusive output (opt-in): bypasses system mixer, source-rate passthrough,
  automatic fallback with reason when the device driver refuses exclusive mode
- Quality ladder: 128K/192K/256K/320K/FLAC-WAV; NetEase honours every tier with
  downward fallback, QQ upgrades to 320K and reports the real bitrate
- GD lyric fallback (opt-in, CC BY-NC): last-resort search+lyric when local tags,
  platform APIs and the NetEase backup all come up empty
- Skin backgrounds no longer wash out; text contrast recalibrated to >=5:1
- Rebrand to Yimai Music; library renamed to local music; version 1.0.0
- 12 functional fixes (kugou cache purge, download overwrite, N+1 queries, ...)
"@
  & $GIT -C $WORK commit -m $msg -m $body
}

& $GIT -C $WORK remote set-url origin "https://$TOK@github.com/$OWNER/$REPO.git"
& $GIT -C $WORK push origin $BRANCH
$rc = $LASTEXITCODE
# 立刻把 remote 还原成不带令牌的干净地址，避免令牌留在 .git/config
& $GIT -C $WORK remote set-url origin $CLEAN_REMOTE
$TOK = $null
if ($rc -ne 0) { Write-Output "PUSH FAILED ($rc)"; exit $rc }
Write-Output "PUSH OK -> $CLEAN_REMOTE"
