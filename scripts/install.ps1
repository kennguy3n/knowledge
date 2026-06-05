#!/usr/bin/env pwsh
#
# Knowledge - one-command installer for SMEs (Windows / PowerShell).
#
# PowerShell equivalent of scripts/install.sh. Gets a fresh host from
# zero to a running stack with sensible defaults:
#   1. checks Docker + the Compose plugin are present,
#   2. generates strong secrets and writes them to `.env` (never
#      clobbering an existing one - rotating the master key would orphan
#      the encrypted store),
#   3. asks whether to enable on-device synthesis (needs 4GB+ RAM),
#   4. pulls the published images and starts the stack,
#   5. waits for the gateway to report healthy,
#   6. prints the URLs to open.
#
# Usage (from a clone):
#   ./scripts/install.ps1
#
# Or straight from the web (downloads compose files into ./knowledge):
#   irm https://raw.githubusercontent.com/kennguy3n/knowledge/main/scripts/install.ps1 | iex
#
# Parameters can also be supplied via environment variables of the same
# name (KNOWLEDGE_SLM_DEVICE_TIER, KNOWLEDGE_HOME, KNOWLEDGE_RAW_BASE,
# KNOWLEDGE_IMAGE_TAG, KNOWLEDGE_INSTALL_DRY_RUN, KNOWLEDGE_ASSUME_YES).

# Write-Host is intentional here: this is an interactive installer whose
# colored status lines are user-facing console output, not pipeline data.
# New-HexSecret generates a value and changes no system state, so the
# ShouldProcess plumbing that rule wants would be noise.
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSAvoidUsingWriteHost', '')]
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSUseShouldProcessForStateChangingFunctions', '')]
[CmdletBinding()]
param(
  # high|medium|low - skips the synthesis prompt when set.
  [string]$DeviceTier = $env:KNOWLEDGE_SLM_DEVICE_TIER,
  # Install dir for the irm|iex path (default ./knowledge).
  [string]$KnowledgeHome = $env:KNOWLEDGE_HOME,
  # Raw repo base URL used to fetch compose files.
  [string]$RawBase = $env:KNOWLEDGE_RAW_BASE,
  # Published image tag to run (default "latest").
  [string]$ImageTag = $env:KNOWLEDGE_IMAGE_TAG,
  # Do everything except `docker compose up` / health wait.
  [switch]$DryRun = ($env:KNOWLEDGE_INSTALL_DRY_RUN -eq '1'),
  # Non-interactive; accept defaults (enables synthesis).
  [switch]$AssumeYes = ($env:KNOWLEDGE_ASSUME_YES -eq '1')
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# -- Output helpers ----------------------------------------------------
function Write-Step($m) { Write-Host "==> $m" -ForegroundColor Blue }
function Write-Ok($m)   { Write-Host "[ok] $m" -ForegroundColor Green }
function Write-Warn($m) { Write-Host "[!] $m"  -ForegroundColor Yellow }
function Die($m) { Write-Host "[x] $m" -ForegroundColor Red; exit 1 }

function Test-Command($name) {
  $null -ne (Get-Command $name -ErrorAction SilentlyContinue)
}

# -- Secret generation -------------------------------------------------
# `n` random bytes as lowercase hex. Uses the .NET CSPRNG so it does not
# depend on openssl being installed on Windows. Hex keeps the value safe
# to embed unescaped in `.env` and in the Postgres URL.
function New-HexSecret([int]$n) {
  $bytes = [byte[]]::new($n)
  [System.Security.Cryptography.RandomNumberGenerator]::Fill($bytes)
  -join ($bytes | ForEach-Object { $_.ToString('x2') })
}

# -- Locate the Compose files ------------------------------------------
# Prefer a local clone (the common case). When run via irm|iex there is
# no on-disk repo, so fetch just the two compose files into
# KnowledgeHome and run the published images from there.
function Resolve-ComposeDir([string]$InstallHome, [string]$RawBaseUrl) {
  $scriptPath = $PSCommandPath
  if ($scriptPath -and (Test-Path $scriptPath)) {
    $repoRoot = Split-Path -Parent (Split-Path -Parent $scriptPath)
    if (Test-Path (Join-Path $repoRoot 'deploy/docker-compose.yml')) {
      return $repoRoot
    }
  }

  $installHome = if ($InstallHome) { $InstallHome } else { Join-Path (Get-Location) 'knowledge' }
  $raw = if ($RawBaseUrl) { $RawBaseUrl } else { 'https://raw.githubusercontent.com/kennguy3n/knowledge/main' }
  $deploy = Join-Path $installHome 'deploy'
  New-Item -ItemType Directory -Force -Path $deploy | Out-Null
  Write-Warn "No local checkout found - downloading compose files into $installHome"
  try {
    Invoke-WebRequest -UseBasicParsing -Uri "$raw/deploy/docker-compose.yml" `
      -OutFile (Join-Path $deploy 'docker-compose.yml')
    Invoke-WebRequest -UseBasicParsing -Uri "$raw/deploy/docker-compose.images.yml" `
      -OutFile (Join-Path $deploy 'docker-compose.images.yml')
  } catch {
    Die "failed to download compose files from ${raw}: $($_.Exception.Message)"
  }
  return $installHome
}

# -- Prerequisite checks -----------------------------------------------
function Test-Docker {
  if (-not (Test-Command 'docker')) {
    Die 'Docker is not installed. Install Docker Desktop first: https://docs.docker.com/get-docker/'
  }
  & docker compose version *> $null
  if ($LASTEXITCODE -ne 0) {
    Die 'The Docker Compose plugin is missing. Install Docker Compose v2: https://docs.docker.com/compose/install/'
  }
  & docker info *> $null
  if ($LASTEXITCODE -ne 0) {
    Die 'Docker is installed but the daemon is not reachable. Start Docker Desktop and re-run.'
  }
  Write-Ok 'Docker and Compose plugin detected'
}

# Lock the secrets file down to its owner, mirroring the bash installer's
# `umask 077`. On Windows this disables ACL inheritance and grants full
# control to the current user only; on pwsh/Unix it chmods the file 600.
# Best-effort: a failure here is warned but never aborts the install, since
# the secrets are already written and this is defense-in-depth.
function Protect-SecretFile([string]$path) {
  $onWindows = if ($PSVersionTable.PSVersion.Major -ge 6) { $IsWindows } else { $true }
  try {
    if ($onWindows) {
      $user = [System.Security.Principal.WindowsIdentity]::GetCurrent().User
      $acl = New-Object System.Security.AccessControl.FileSecurity
      $acl.SetOwner($user)
      # Disable inheritance and drop inherited ACEs so the file is not
      # exposed by a permissive parent directory.
      $acl.SetAccessRuleProtection($true, $false)
      $rule = New-Object System.Security.AccessControl.FileSystemAccessRule(
        $user, 'FullControl', 'Allow')
      $acl.AddAccessRule($rule)
      [System.IO.File]::SetAccessControl($path, $acl)
    } else {
      & chmod 600 $path
    }
  } catch {
    Write-Warn "could not restrict permissions on ${path}: $($_.Exception.Message)"
  }
}

# -- .env generation ---------------------------------------------------
function Write-EnvFile([string]$envFile, [string]$tier, [string]$tagOverride) {
  if (Test-Path $envFile) {
    Write-Warn "$envFile already exists - keeping existing secrets (delete it to regenerate)."
    return
  }

  Write-Step "Generating secrets and writing $envFile"
  $master   = New-HexSecret 32   # 64 hex chars - SQLCipher master key.
  $pgPw     = New-HexSecret 24
  $minioPw  = New-HexSecret 24
  $gfPw     = New-HexSecret 18
  $tag = if ($tagOverride) { $tagOverride } else { 'latest' }
  $stamp = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')

  # Single-quoted here-string so nothing is interpolated by PowerShell;
  # placeholders are substituted explicitly below.
  $template = @'
# Generated by scripts/install.ps1 on __STAMP__.
# Secrets are unique to this deployment. Keep this file private and back
# up KNOWLEDGE_MASTER_KEY - losing it makes the encrypted store
# unrecoverable.

# -- Substrate --------------------------------------------------------
KNOWLEDGE_MASTER_KEY=__MASTER__
# Enables on-device SLM synthesis at the "high" tier; "medium"/"low"
# disable it (classification still runs on the free fallback adapter).
KNOWLEDGE_SLM_DEVICE_TIER=__TIER__

# -- Managed-cloud synthesis (optional) -------------------------------
# Set the URL to route synthesis to an external OpenAI-compatible
# endpoint instead of the local SLM. See docs/operator/configuration.md.
KNOWLEDGE_MANAGED_INFERENCE_URL=
KNOWLEDGE_MANAGED_INFERENCE_KEY=
KNOWLEDGE_MANAGED_INFERENCE_MODEL=gpt-4o-mini

# -- Gateway ----------------------------------------------------------
# Left empty for a frictionless localhost start: the admin SPA then
# calls the gateway without a token. Set a value before exposing the
# gateway off-host.
KNOWLEDGE_API_KEY=
KNOWLEDGE_JWT_SECRET=
KNOWLEDGE_PUBLIC_BASE_URL=http://localhost:8080

# -- Postgres ---------------------------------------------------------
POSTGRES_USER=knowledge
POSTGRES_PASSWORD=__PGPW__
POSTGRES_DB=knowledge

# -- MinIO ------------------------------------------------------------
MINIO_ROOT_USER=knowledge
MINIO_ROOT_PASSWORD=__MINIOPW__
MINIO_BUCKET=knowledge

# -- Grafana ----------------------------------------------------------
GF_ADMIN_USER=admin
GF_ADMIN_PASSWORD=__GFPW__

# -- Published image tag ----------------------------------------------
# The base compose file keys its image refs on KNOWLEDGE_IMAGE_TAG; the
# published-images overlay (deploy/docker-compose.images.yml) keys on
# KNOWLEDGE_VERSION. This installer layers both files, so pin both to the
# same tag to keep the whole stack on one version.
KNOWLEDGE_IMAGE_TAG=__TAG__
KNOWLEDGE_VERSION=__TAG__
'@

  $content = $template.
    Replace('__STAMP__',  $stamp).
    Replace('__MASTER__', $master).
    Replace('__TIER__',   $tier).
    Replace('__PGPW__',   $pgPw).
    Replace('__MINIOPW__',$minioPw).
    Replace('__GFPW__',   $gfPw).
    Replace('__TAG__',    $tag)

  # Write UTF-8 without BOM so Docker Compose parses the file cleanly.
  $utf8NoBom = [System.Text.UTF8Encoding]::new($false)
  [System.IO.File]::WriteAllText($envFile, ($content -replace "`r`n", "`n"), $utf8NoBom)
  Protect-SecretFile $envFile
  Write-Ok "Wrote $envFile"
}

# -- Synthesis prompt --------------------------------------------------
function Resolve-DeviceTier([string]$PresetTier, [bool]$NonInteractive) {
  if ($PresetTier) { return $PresetTier }
  if ($NonInteractive) { return 'high' }
  if (-not [Environment]::UserInteractive) {
    Write-Warn 'Non-interactive shell and no -DeviceTier set - defaulting to synthesis enabled (high).'
    return 'high'
  }
  $reply = Read-Host 'Enable on-device synthesis? It produces summaries/concepts but needs 4GB+ RAM. [Y/n]'
  if ($reply -match '^[Nn]') { return 'medium' } else { return 'high' }
}

# -- Health wait -------------------------------------------------------
function Wait-ForHealth([int]$port) {
  $url = "http://localhost:$port/health"
  Write-Step "Waiting for the gateway to become healthy at $url"
  for ($i = 1; $i -le 60; $i++) {
    try {
      Invoke-WebRequest -UseBasicParsing -Uri $url -TimeoutSec 3 *> $null
      Write-Ok 'Gateway is healthy'
      return $true
    } catch {
      Start-Sleep -Seconds 2
    }
  }
  Write-Warn "Gateway did not report healthy within 120s. Check 'docker compose logs -f'."
  return $false
}

# -- Read a var back out of the generated .env -------------------------
function Get-EnvValue([string]$envFile, [string]$key, [string]$default) {
  $line = Select-String -Path $envFile -Pattern "^$key=" -ErrorAction SilentlyContinue |
    Select-Object -Last 1
  if ($line) { return ($line.Line -replace "^$key=", '') }
  return $default
}

function Main {
  param(
    [string]$DeviceTier,
    [string]$KnowledgeHome,
    [string]$RawBase,
    [string]$ImageTag,
    [bool]$DryRun,
    [bool]$AssumeYes
  )
  Write-Host 'Knowledge installer' -ForegroundColor White
  Write-Host ''
  Test-Docker

  $composeDir = Resolve-ComposeDir -InstallHome $KnowledgeHome -RawBaseUrl $RawBase
  $envFile = Join-Path $composeDir '.env'
  Write-Step "Using compose files in $(Join-Path $composeDir 'deploy')"

  $tier = Resolve-DeviceTier -PresetTier $DeviceTier -NonInteractive $AssumeYes
  switch ($tier) {
    'high'   { Write-Ok 'Synthesis enabled (device tier: high)' }
    'medium' { Write-Ok 'Synthesis disabled (device tier: medium)' }
    'low'    { Write-Ok 'Synthesis disabled (device tier: low)' }
    default  { Die "invalid device tier '$tier' (expected high|medium|low)" }
  }

  Write-EnvFile -envFile $envFile -tier $tier -tagOverride $ImageTag

  $gatewayPort = Get-EnvValue -envFile $envFile -key 'GATEWAY_PORT' -default '8080'
  $adminPort   = Get-EnvValue -envFile $envFile -key 'ADMIN_PORT'   -default '3001'
  $grafanaPort = Get-EnvValue -envFile $envFile -key 'GRAFANA_PORT' -default '3000'

  # Pass --env-file explicitly: Compose resolves a bare `.env` relative
  # to the compose file's directory (deploy/), not the repo root where
  # we write it, so without this KNOWLEDGE_MASTER_KEY comes through blank.
  Push-Location $composeDir
  try {
    if ($DryRun) {
      Write-Warn 'Dry run - validating compose config, skipping "up".'
      & docker compose --env-file $envFile `
        -f deploy/docker-compose.yml `
        -f deploy/docker-compose.images.yml `
        config *> $null
      if ($LASTEXITCODE -ne 0) { Die 'docker compose config failed' }
      Write-Ok 'Compose configuration is valid'
    } else {
      Write-Step 'Pulling images and starting the stack (this can take a few minutes)...'
      & docker compose --env-file $envFile `
        -f deploy/docker-compose.yml `
        -f deploy/docker-compose.images.yml `
        up -d
      if ($LASTEXITCODE -ne 0) { Die 'docker compose up failed' }
      [void](Wait-ForHealth ([int]$gatewayPort))
    }
  } finally {
    Pop-Location
  }

  Write-Host ''
  Write-Host 'Knowledge is running.' -ForegroundColor Green
  Write-Host "  Admin:   http://localhost:$adminPort"
  Write-Host "  API:     http://localhost:$gatewayPort"
  Write-Host "  Grafana: http://localhost:$grafanaPort (user: admin)"
  Write-Host ''
  Write-Host 'Open the Admin URL to finish setup with the first-run wizard.'
}

# Thread the script parameters explicitly into Main so they are
# referenced at script scope (and so Main carries no hidden global
# state). PSBoundParameters is not used because switches default from
# environment variables above and may not be "bound".
Main -DeviceTier $DeviceTier -KnowledgeHome $KnowledgeHome -RawBase $RawBase `
  -ImageTag $ImageTag -DryRun $DryRun -AssumeYes $AssumeYes
