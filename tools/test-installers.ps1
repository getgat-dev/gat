# Exercise both Windows PowerShell 5.1 and PowerShell 7 with local releases.
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSAvoidOverwritingBuiltInCmdlets', '',
  Justification = 'Fixture-local mocks intercept downloads and copies without changing the installer API.')]
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSUseShouldProcessForStateChangingFunctions', '',
  Justification = 'The Start-Sleep mock records retry delays and performs no state-changing operation.')]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2.0
$installer = Join-Path $PSScriptRoot '../docs/install.ps1'
$fixtureRoot = Join-Path ([IO.Path]::GetTempPath()) ([IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Path $fixtureRoot | Out-Null
$savedEnvironment = @{}
foreach ($name in @('GAT_INSTALL_DIR', 'GAT_VERSION', 'PROCESSOR_ARCHITECTURE', 'PROCESSOR_ARCHITEW6432', 'FIXTURE_SCENARIO', 'TEMP', 'TMP')) {
  $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name)
}

function Invoke-WebRequest {
  param([string]$Uri, [string]$OutFile, [switch]$UseBasicParsing, [int]$TimeoutSec)
  if (-not $UseBasicParsing -or $TimeoutSec -ne 120) { throw 'request policy missing' }
  $fixtureState.Requests++
  $baseUrl = 'https://github.com/getgat-dev/gat/releases/download/v0.1.0+build.1'
  if ($Uri -ceq "$baseUrl/SHA256SUMS") {
    if ($env:FIXTURE_SCENARIO -eq 'checksum_download_failure') { throw 'checksum request failed' }
    Microsoft.PowerShell.Management\Copy-Item -LiteralPath (Join-Path $fixtureRoot 'SHA256SUMS') -Destination $OutFile
  } elseif ($Uri -ceq "$baseUrl/$fixtureArchive") {
    $fixtureState.ArchiveRequests++
    if ($env:FIXTURE_SCENARIO -eq 'download_failure') {
      [IO.File]::WriteAllText($OutFile, 'partial download')
      throw 'archive request failed'
    }
    if ($env:FIXTURE_SCENARIO -eq 'http_not_found' -or
        ($env:FIXTURE_SCENARIO -eq 'http_retry_success' -and $fixtureState.ArchiveRequests -eq 1)) {
      $status = if ($env:FIXTURE_SCENARIO -eq 'http_not_found') { 404 } else { 503 }
      $response = [FixtureHttpResponse]::new($status)
      throw [Net.WebException]::new('HTTP failure', $null, [Net.WebExceptionStatus]::ProtocolError, $response)
    }
    if ($env:FIXTURE_SCENARIO -eq 'http_certificate_failure') {
      throw [Net.Http.HttpRequestException]::new('TLS failure', [Security.Authentication.AuthenticationException]::new('certificate invalid'))
    }
    if ($env:FIXTURE_SCENARIO -eq 'http_exception_retry' -and $fixtureState.ArchiveRequests -eq 1) {
      throw [Net.Http.HttpRequestException]::new('connection interrupted')
    }
    if ($env:FIXTURE_SCENARIO -eq 'certificate_failure') {
      throw [Net.WebException]::new('certificate validation failed', [Net.WebExceptionStatus]::TrustFailure)
    }
    if ($env:FIXTURE_SCENARIO -eq 'retry_exhausted' -or
        ($env:FIXTURE_SCENARIO -eq 'retry_success' -and $fixtureState.ArchiveRequests -eq 1)) {
      [IO.File]::WriteAllText($OutFile, 'partial download')
      throw [Net.WebException]::new('connection interrupted', [Net.WebExceptionStatus]::ConnectionClosed)
    }
    Microsoft.PowerShell.Management\Copy-Item -LiteralPath (Join-Path $fixtureRoot $fixtureArchive) -Destination $OutFile
  } else {
    throw "unexpected download: $Uri"
  }
}

function Invoke-RestMethod {
  param([string]$Uri, [int]$TimeoutSec)
  if ($TimeoutSec -ne 120) { throw 'request timeout missing' }
  if ($Uri -cne 'https://api.github.com/repos/getgat-dev/gat/releases/latest') { throw "unexpected API: $Uri" }
  $fixtureState.Requests++
  if ($env:FIXTURE_SCENARIO -eq 'latest_failure') { throw 'latest request failed' }
  if ($env:FIXTURE_SCENARIO -eq 'latest_missing') { return @{ id = 1 } }
  # Compact JSON with a preceding property must not affect latest resolution.
  return ('{"id":1,"tag_name":"v0.1.0+build.1"}' | ConvertFrom-Json)
}

function Start-Sleep {
  param([int]$Seconds)
  $fixtureState.RetryDelays += $Seconds
}

function Copy-Item {
  param([string]$LiteralPath, [string]$Destination)
  if ($env:FIXTURE_SCENARIO -in @('copy_failure', 'readonly_copy_failure') -and [IO.Path]::GetFileName($Destination).StartsWith('.gat-install.')) {
    [IO.File]::WriteAllText($Destination, 'partial executable')
    if ($env:FIXTURE_SCENARIO -eq 'readonly_copy_failure') {
      [IO.File]::SetAttributes($Destination, [IO.FileAttributes]::ReadOnly)
    }
    throw 'staged copy failed'
  }
  Microsoft.PowerShell.Management\Copy-Item -LiteralPath $LiteralPath -Destination $Destination
}

# Replace the transport boundary in a parsed copy; real transport tests below
# exercise network deadlines independently of the release fixtures.
$tokens = $null
$parseErrors = $null
$installerAst = [Management.Automation.Language.Parser]::ParseFile($installer, [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count) { throw $parseErrors[0] }
$transport = $installerAst.Find({ param($node)
  $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Invoke-ReleaseRequest'
}, $true)
$mockTransport = @'
function Invoke-ReleaseRequest {
  param([string]$Uri, [string]$OutFile)
  if ($OutFile) { Invoke-WebRequest -UseBasicParsing -Uri $Uri -OutFile $OutFile -TimeoutSec 120 }
  else { Invoke-RestMethod -Uri $Uri -TimeoutSec 120 }
}
'@
$installerText = [IO.File]::ReadAllText($installer)
$installerBlock = [scriptblock]::Create($installerText.Remove($transport.Extent.StartOffset,
  $transport.Extent.EndOffset - $transport.Extent.StartOffset).Insert($transport.Extent.StartOffset, $mockTransport))

try {
  # Use a fresh shell so download/retry mocks cannot affect real HTTP tests.
  $shellName = if ($PSVersionTable.PSEdition -eq 'Core') { 'pwsh.exe' } else { 'powershell.exe' }
  & (Join-Path $PSHOME $shellName) -NoProfile -File (Join-Path $PSScriptRoot 'test-installer-network.ps1')
  if ($LASTEXITCODE -ne 0) { throw 'real HTTP transport tests failed' }
  Add-Type -AssemblyName System.Net.Http
  Add-Type -TypeDefinition @'
public sealed class FixtureHttpResponse : System.Net.WebResponse {
  public System.Net.HttpStatusCode StatusCode { get; private set; }
  public FixtureHttpResponse(int status) { StatusCode = (System.Net.HttpStatusCode)status; }
}
'@
  $tempDir = Join-Path $fixtureRoot 'tmp'
  New-Item -ItemType Directory -Path $tempDir | Out-Null
  $env:TEMP = $tempDir
  $env:TMP = $tempDir
  # Compile a real executable so the installer runs its actual smoke test.
  # Framework csc is present on both Windows runner shell baselines.
  $source = Join-Path $fixtureRoot 'fixture.cs'
  [IO.File]::WriteAllText($source, @'
using System;
class Fixture {
  static int Main(string[] args) {
    if (args.Length == 1 && args[0] == "--hold-pipes") { System.Threading.Thread.Sleep(60000); return 0; }
    if (Environment.GetEnvironmentVariable("FIXTURE_SCENARIO") == "inherited_pipes") {
      var info = new System.Diagnostics.ProcessStartInfo(System.Reflection.Assembly.GetExecutingAssembly().Location, "--hold-pipes");
      info.UseShellExecute = false;
      var child = System.Diagnostics.Process.Start(info);
      System.IO.File.WriteAllText(System.IO.Path.Combine(Environment.GetEnvironmentVariable("GAT_INSTALL_DIR"), "child.pid"), child.Id.ToString());
    }
    if (args.Length != 1 || args[0] != "--version") return 2;
    if (Environment.GetEnvironmentVariable("FIXTURE_SCENARIO") == "incompatible") return 1;
    if (Environment.GetEnvironmentVariable("FIXTURE_SCENARIO") == "hung_payload") System.Threading.Thread.Sleep(60000);
    if (Environment.GetEnvironmentVariable("FIXTURE_SCENARIO") == "wrong_version") {
      Console.WriteLine("gat 9.9.9"); return 0;
    }
    Console.WriteLine("gat 0.1.0+build.1");
    return 0;
  }
}
'@)
  $fixtureExe = Join-Path $fixtureRoot 'fixture.exe'
  & (Join-Path $env:WINDIR 'Microsoft.NET\Framework64\v4.0.30319\csc.exe') /nologo /target:exe "/out:$fixtureExe" $source
  if ($LASTEXITCODE -ne 0) { throw 'could not compile executable fixture' }
  $payloadHash = (Get-FileHash -LiteralPath $fixtureExe).Hash

  foreach ($architecture in @('AMD64', 'ARM64')) {
    # The archive routing is simulated; the executable runs on the host CPU.
    $env:PROCESSOR_ARCHITECTURE = $architecture
    $env:PROCESSOR_ARCHITEW6432 = ''
    $cpu = if ($architecture -eq 'ARM64') { 'aarch64' } else { 'x86_64' }
    $staging = "gat-v0.1.0+build.1-$cpu-pc-windows-msvc"
    $fixtureArchive = "$staging.zip"
    $sourceDir = Join-Path $fixtureRoot $staging
    New-Item -ItemType Directory -Path $sourceDir | Out-Null
    Microsoft.PowerShell.Management\Copy-Item -LiteralPath $fixtureExe -Destination (Join-Path $sourceDir 'gat.exe')
    $archivePath = Join-Path $fixtureRoot $fixtureArchive
    Compress-Archive -LiteralPath $sourceDir -DestinationPath $archivePath
    $validArchive = [IO.File]::ReadAllBytes($archivePath)

    foreach ($scenario in @('valid', 'missing', 'duplicate', 'malformed', 'short_hash', 'malformed_duplicate', 'mismatch', 'similar_name', 'crlf', 'uppercase_hash',
        'upgrade', 'latest', 'latest_failure', 'latest_missing', 'download_failure', 'checksum_download_failure', 'certificate_failure', 'retry_success', 'retry_exhausted', 'http_retry_success', 'http_not_found', 'http_exception_retry', 'http_certificate_failure',
        'invalid_archive', 'incompatible', 'hung_payload', 'inherited_pipes', 'wrong_version', 'copy_failure', 'readonly_copy_failure', 'locked_destination', 'directory', 'multiline_prefix', 'multiline_suffix', 'trailing_newline',
        'carriage_return', 'double_prefix', 'empty_version', 'unknown_option', 'env_version', 'relative_path', 'scoped_success', 'scoped_failure')) {
      $env:FIXTURE_SCENARIO = $scenario
      $env:GAT_VERSION = ''
      $fixtureState = @{ Requests = 0; ArchiveRequests = 0; RetryDelays = @() }
      [IO.File]::WriteAllBytes($archivePath, $validArchive)
      if ($scenario -eq 'invalid_archive') { [IO.File]::WriteAllText($archivePath, 'not an archive') }
      $hash = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
      $entry = "$hash  $fixtureArchive`n"
      $record = switch ($scenario) {
        'missing' { '' }
        'duplicate' { $entry + $entry }
        'malformed' { "$('z' * 64)  $fixtureArchive`n" }
        'short_hash' { "abc  $fixtureArchive`n" }
        'malformed_duplicate' { $entry + "abc  $fixtureArchive`n" }
        'mismatch' { "$('0' * 64)  $fixtureArchive`n" }
        'similar_name' { "$hash  $fixtureArchive.extra`n" }
        'crlf' { "$hash  $fixtureArchive`r`n" }
        'uppercase_hash' { "$($hash.ToUpperInvariant())  $fixtureArchive`n" }
        default { $entry }
      }
      [IO.File]::WriteAllText((Join-Path $fixtureRoot 'SHA256SUMS'), "$hash  unrelated.zip`n$record", [Text.Encoding]::ASCII)
      # Brackets and spaces exercise literal path handling in PowerShell.
      $destination = Join-Path $fixtureRoot "install [$architecture $scenario]"
      New-Item -ItemType Directory -Path $destination | Out-Null
      $installedFile = Join-Path $destination 'gat.exe'
      if ($scenario -eq 'directory') {
        New-Item -ItemType Directory -Path $installedFile | Out-Null
      } elseif ($scenario -notin @('valid', 'crlf', 'uppercase_hash')) {
        [IO.File]::WriteAllText($installedFile, 'old binary')
      }
      $env:GAT_INSTALL_DIR = $destination
      $arguments = @{ Version = '0.1.0+build.1' }
      switch ($scenario) {
        { $_ -like 'latest*' } { $arguments = @{} }
        'multiline_prefix' { $arguments.Version = "bad`n0.1.0" }
        'multiline_suffix' { $arguments.Version = "0.1.0`n../../bad" }
        'trailing_newline' { $arguments.Version = "0.1.0`n" }
        'carriage_return' { $arguments.Version = "0.1.0`r" }
        'double_prefix' { $arguments.Version = 'vV0.1.0' }
        'empty_version' { $arguments.Version = '' }
        'unknown_option' { $arguments = @{ Versoin = '0.1.0' } }
        'scoped_success' { $env:GAT_VERSION = '0.1.0+build.1' }
        'scoped_failure' { $env:GAT_VERSION = 'invalid' }
        'env_version' { $arguments = @{}; $env:GAT_VERSION = 'V0.1.0+build.1' }
        'relative_path' { $env:GAT_INSTALL_DIR = '.\' + [IO.Path]::GetFileName($destination) }
      }
      $lock = $null
      if ($scenario -eq 'locked_destination') {
        $lock = [IO.File]::Open($installedFile, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
      }
      $failure = $null
      Push-Location -LiteralPath $fixtureRoot
      try {
        if ($scenario -like 'scoped_*') {
          & {
            param([string]$Version)
            $ErrorActionPreference = 'Continue'
            $Repo = 'caller repository'
            function Get-ReleaseResource { 'caller helper' }
            try {
              & $installerBlock
            } finally {
              if ($Version -cne 'caller version' -or $ErrorActionPreference -cne 'Continue' -or $Repo -cne 'caller repository' -or
                  (Get-ReleaseResource) -cne 'caller helper') {
                throw 'installer changed caller state through scoped invocation'
              }
            }
          } -Version 'caller version'
        } else {
          & $installerBlock @arguments
        }
      } catch {
        $failure = $_
      } finally {
        Pop-Location
        if ($lock) { $lock.Dispose() }
        $childPidPath = Join-Path $destination 'child.pid'
        if (Test-Path -LiteralPath $childPidPath) {
          $child = Get-Process -Id ([int][IO.File]::ReadAllText($childPidPath)) -ErrorAction SilentlyContinue
          if ($null -ne $child -and -not $child.WaitForExit(1000)) {
            Stop-Process -Id $child.Id -ErrorAction SilentlyContinue
            throw 'installer left a probe descendant running'
          }
        }
      }
      if ($scenario -in @('valid', 'crlf', 'uppercase_hash', 'upgrade', 'latest', 'retry_success', 'http_retry_success', 'http_exception_retry', 'env_version', 'relative_path', 'scoped_success')) {
        if ($null -ne $failure) { throw $failure }
        if ((Get-FileHash -LiteralPath $installedFile).Hash -ne $payloadHash) { throw "installed payload differs for $scenario" }
      } else {
        if ($null -eq $failure) { throw "installer accepted $architecture $scenario" }
        if ($scenario -eq 'directory') {
          if (-not (Test-Path -LiteralPath $installedFile -PathType Container) -or
              (Test-Path -LiteralPath (Join-Path $installedFile 'gat.exe'))) { throw 'destination directory modified' }
        } elseif ([IO.File]::ReadAllText($installedFile) -cne 'old binary') {
          throw "old installation damaged by $scenario"
        }
      }
      if ($scenario -eq 'unknown_option' -and
          ($fixtureState.Requests -ne 0 -or $failure.Exception -isnot [System.Management.Automation.ParameterBindingException])) { throw 'invalid arguments reached installer' }
      if ($scenario -in @('hung_payload', 'inherited_pipes') -and $failure.Exception.Message -notmatch 'gat startup timed out') { throw $failure }
      if ($scenario -eq 'wrong_version' -and $failure.Exception.Message -notmatch 'reported an unexpected version') { throw $failure }
      if ($scenario -eq 'incompatible' -and $failure.Exception.Message -notmatch 'downloaded gat cannot run') { throw $failure }
      if ($scenario -in @('copy_failure', 'readonly_copy_failure') -and $failure.Exception.Message -notmatch 'staged copy failed') { throw $failure }
      if ($scenario -eq 'locked_destination' -and $failure.Exception.InnerException -isnot [IO.IOException]) { throw $failure }
      if ($scenario -in @('missing', 'duplicate', 'malformed', 'short_hash', 'malformed_duplicate', 'mismatch', 'similar_name') -and
          $failure.Exception.Message -notmatch 'expected exactly one valid checksum|checksum verification failed') { throw $failure }
      if ($scenario -in @('multiline_prefix', 'multiline_suffix', 'trailing_newline', 'carriage_return', 'double_prefix', 'empty_version', 'scoped_failure')) {
        if ($fixtureState.Requests -ne 0 -or $failure.Exception.Message -notmatch 'invalid version') { throw 'invalid version reached network' }
      }
      if ($scenario -in @('certificate_failure', 'http_not_found', 'http_certificate_failure') -and $fixtureState.ArchiveRequests -ne 1) { throw 'certificate error was retried' }
      if ($scenario -in @('retry_success', 'http_retry_success', 'http_exception_retry') -and ($fixtureState.ArchiveRequests -ne 2 -or ($fixtureState.RetryDelays -join ',') -ne '1')) { throw 'retry did not recover' }
      if ($scenario -eq 'retry_exhausted' -and ($fixtureState.ArchiveRequests -ne 3 -or ($fixtureState.RetryDelays -join ',') -ne '1,2')) { throw 'retry budget not enforced' }
      if ($scenario -in @('download_failure', 'checksum_download_failure', 'latest_failure', 'certificate_failure', 'http_not_found', 'http_certificate_failure')) {
        if ($fixtureState.RetryDelays.Count -ne 0 -or $failure.Exception.Message -notmatch 'failed to download https://') { throw 'permanent error lost context or retried' }
      }
      if (@(Get-ChildItem -LiteralPath $destination -Force -Filter '.gat-install.*').Count -ne 0) { throw 'staged executable leaked' }
      if (@(Get-ChildItem -LiteralPath $tempDir -Force).Count -ne 0) { throw ('temporary download leaked: ' + ((Get-ChildItem -LiteralPath $tempDir -Force).Name -join ', ')) }
      Write-Output "PASS: $architecture $scenario"
    }
  }
} finally {
  foreach ($name in $savedEnvironment.Keys) { [Environment]::SetEnvironmentVariable($name, $savedEnvironment[$name]) }
  Remove-Item -LiteralPath $fixtureRoot -Recurse -Force
}
