#!/usr/bin/env pwsh
#Requires -Version 5.1
# Download, verify, and install a prebuilt gat release.
# Served from main via the redirect in docs/docs.json.
# Pin a version with the option below or GAT_VERSION; otherwise use latest.
#
# Usage:
#   & ([scriptblock]::Create((irm https://getgat.dev/install.ps1)))
#
#   & ([scriptblock]::Create((irm https://getgat.dev/install.ps1))) -Version 0.1.0
[CmdletBinding()]
param(
  [string]$Version = ""
)

# Keep installer state and helper functions local to this invocation.
& {
  param([string]$Version, [bool]$VersionSpecified)

  $ErrorActionPreference = "Stop"

  if ($VersionSpecified -and [string]::IsNullOrEmpty($Version)) {
    throw "invalid version: -Version requires a non-empty argument"
  }
  $Repo = "getgat-dev/gat"
  if ([string]::IsNullOrEmpty($Version)) {
    $Version = $env:GAT_VERSION
  }

  # Official SemVer 2.0.0 grammar (https://semver.org), without the leading
  # "v" (which is stripped and re-added separately).
  $SemVerPattern = '\A(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-((0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)(\.(0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*))*))?(\+([0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*))?\z'

  function Resolve-ReleaseRedirect {
    param([Uri]$Source, [string]$Location)
    if ([string]::IsNullOrWhiteSpace($Location)) { throw 'release redirect has no location' }
    $target = [Uri]::new($Source, $Location)
    if ($target.Scheme -ne 'https' -and
        -not ($Source.Scheme -eq 'http' -and $target.Scheme -eq 'http')) {
      throw 'release redirect would weaken or change the transport'
    }
    return $target
  }

  function Invoke-ReleaseRequest {
    param([string]$Uri, [string]$OutFile, [int]$TimeoutMilliseconds = 120000)

    # Web cmdlet timeouts differ between PowerShell versions and may cover only
    # connection establishment. Bound every asynchronous network operation by
    # the same deadline, including response-body reads.
    $deadline = [Diagnostics.Stopwatch]::StartNew()
    $currentUri = [Uri]$Uri
    $request = $null
    $response = $null
    $inputStream = $null
    $outputStream = $null
    try {
      # Framework and modern .NET differ on HTTPS-to-HTTP redirects. Follow
      # redirects ourselves and keep every hop inside the original deadline.
      for ($redirects = 0; ; $redirects++) {
        if ($deadline.ElapsedMilliseconds -ge $TimeoutMilliseconds) {
          throw [Net.WebException]::new('download timed out', [Net.WebExceptionStatus]::Timeout)
        }
        $request = [Net.WebRequest]::Create($currentUri)
        $request.UserAgent = 'gat-installer'
        $request.AllowAutoRedirect = $false
        $pending = $request.GetResponseAsync()
        if (-not $pending.Wait([Math]::Max(0, $TimeoutMilliseconds - [int]$deadline.ElapsedMilliseconds))) {
          throw [Net.WebException]::new('download timed out', [Net.WebExceptionStatus]::Timeout)
        }
        $response = $pending.GetAwaiter().GetResult()
        if ([int]$response.StatusCode -notin @(301, 302, 303, 307, 308)) { break }
        if ($redirects -ge 10) { throw 'too many release redirects' }
        $currentUri = Resolve-ReleaseRedirect -Source $currentUri -Location $response.Headers['Location']
        $request.Abort()
        $response.Dispose()
        $response = $null
      }
      $inputStream = $response.GetResponseStream()
      $outputStream = if ($OutFile) {
        [IO.File]::Create($OutFile)
      } else {
        [IO.MemoryStream]::new()
      }
      $buffer = New-Object byte[] 81920
      while ($true) {
        if ($deadline.ElapsedMilliseconds -ge $TimeoutMilliseconds) {
          throw [Net.WebException]::new('download timed out', [Net.WebExceptionStatus]::Timeout)
        }
        try {
          $pending = $inputStream.ReadAsync($buffer, 0, $buffer.Length)
          if (-not $pending.Wait([Math]::Max(0, $TimeoutMilliseconds - [int]$deadline.ElapsedMilliseconds))) {
            throw [Net.WebException]::new('download timed out', [Net.WebExceptionStatus]::Timeout)
          }
          $count = $pending.GetAwaiter().GetResult()
        } catch {
          $cause = $_.Exception
          while (($cause -is [Management.Automation.MethodInvocationException] -or
                  $cause -is [AggregateException]) -and $null -ne $cause.InnerException) {
            $cause = $cause.InnerException
          }
          # Normalize network-stream failures only. File creation/writes remain
          # outside this catch so disk errors never consume the retry budget.
          if ($cause -is [IO.IOException]) {
            throw [Net.WebException]::new('response body interrupted', $cause,
              [Net.WebExceptionStatus]::ReceiveFailure, $null)
          }
          throw
        }
        if ($count -eq 0) { break }
        $outputStream.Write($buffer, 0, $count)
      }
      if (-not $OutFile) {
        [Text.Encoding]::UTF8.GetString($outputStream.ToArray()) | ConvertFrom-Json
      }
    } finally {
      # Abort pending I/O before disposing its streams; never wait for the peer.
      if ($null -ne $request) { $request.Abort() }
      if ($null -ne $inputStream) { $inputStream.Dispose() }
      if ($null -ne $outputStream) { $outputStream.Dispose() }
      if ($null -ne $response) { $response.Dispose() }
    }
  }

  function Get-ReleaseResource {
    param([string]$Uri, [string]$OutFile)

    for ($attempt = 0; ; $attempt++) {
      try {
        Invoke-ReleaseRequest -Uri $Uri -OutFile $OutFile
        return
      } catch {
        # Async .NET failures arrive inside PowerShell invocation/aggregate wrappers.
        $failure = $_.Exception
        while (($failure -is [Management.Automation.MethodInvocationException] -or
                $failure -is [AggregateException]) -and $null -ne $failure.InnerException) {
          $failure = $failure.InnerException
        }
        $responseProperty = $failure.PSObject.Properties['Response']
        $response = if ($null -ne $responseProperty) { $responseProperty.Value } else { $null }
        $status = if ($null -ne $response) { [int]$response.StatusCode } else { 0 }
        # Certificate and TLS negotiation failures need user intervention, not
        # retries. Windows PowerShell exposes these through WebException.Status.
        $networkFailure = if ($failure -is [Net.WebException]) {
          $failure.Status -in @(
            [Net.WebExceptionStatus]::Timeout, [Net.WebExceptionStatus]::ConnectFailure,
            [Net.WebExceptionStatus]::ConnectionClosed, [Net.WebExceptionStatus]::ReceiveFailure,
            [Net.WebExceptionStatus]::SendFailure, [Net.WebExceptionStatus]::KeepAliveFailure,
            [Net.WebExceptionStatus]::NameResolutionFailure, [Net.WebExceptionStatus]::ProxyNameResolutionFailure
          )
        } else {
          $failure.GetType().FullName -in @(
            'System.Net.Http.HttpRequestException', 'System.Threading.Tasks.TaskCanceledException'
          ) -and $failure.InnerException -isnot [System.Security.Authentication.AuthenticationException]
        }
        $transient = $status -in @(408, 429, 500, 502, 503, 504) -or ($status -eq 0 -and $networkFailure)
        if (-not $transient -or $attempt -ge 2) {
          throw "failed to download ${Uri}: $($failure.Message)"
        }
        Start-Sleep -Seconds ($attempt + 1)
      }
    }
  }

  function Assert-InstallDestination {
    param([string]$Path)
    if (Test-Path -LiteralPath $Path) {
      $item = Get-Item -LiteralPath $Path -Force
      if ($item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw "destination is a directory or reparse point: $Path"
      }
    }
  }

  if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw "this installer requires Windows; use install.sh on macOS or Linux"
  }

  if (-not [Environment]::Is64BitOperatingSystem) {
    throw "unsupported architecture: 32-bit Windows is not supported"
  }

  $arch = if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64" -or $env:PROCESSOR_ARCHITEW6432 -eq "ARM64") {
    "aarch64"
  } else {
    "x86_64"
  }

  $InstallDir = if ($env:GAT_INSTALL_DIR) { $env:GAT_INSTALL_DIR } else { Join-Path $env:USERPROFILE ".local\bin" }

  $target = "$arch-pc-windows-msvc"
  # Resolve relative overrides once; .NET file operations do not use PowerShell's
  # current location when resolving relative paths.
  $InstallDir = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($InstallDir)
  $destination = Join-Path $InstallDir "gat.exe"
  Assert-InstallDestination $destination

  if ([string]::IsNullOrEmpty($Version)) {
    $release = Get-ReleaseResource -Uri "https://api.github.com/repos/$Repo/releases/latest"
    $tagProperty = if ($null -ne $release) { $release.PSObject.Properties['tag_name'] } else { $null }
    if ($null -eq $tagProperty -or [string]::IsNullOrEmpty($tagProperty.Value)) {
      throw "could not resolve the latest gat release"
    }
    $Version = $tagProperty.Value
  }

  # Normalize both "0.1.0" and "v0.1.0" to a bare SemVer core, then strictly
  # validate it before it is ever used to build a URL or path. Fail closed on
  # anything that isn't a well-formed SemVer version.
  $core = $Version -replace '^[vV]', ''
  if ($core -cnotmatch $SemVerPattern) {
    throw "invalid version: '$Version' (expected SemVer, e.g. 0.1.0 or v0.1.0)"
  }
  $tag = "v$core"

  $archive = "gat-$tag-$target.zip"
  $baseUrl = "https://github.com/$Repo/releases/download/$tag"
  $workdir = Join-Path ([System.IO.Path]::GetTempPath()) ([System.IO.Path]::GetRandomFileName())
  New-Item -ItemType Directory -Path $workdir | Out-Null

  $stagedPath = $null
  try {
    Write-Information -InformationAction Continue "Downloading gat $tag for $target..."
    $archivePath = Join-Path $workdir $archive
    $checksumPath = Join-Path $workdir "SHA256SUMS"
    Get-ReleaseResource -Uri "$baseUrl/$archive" -OutFile $archivePath
    Get-ReleaseResource -Uri "$baseUrl/SHA256SUMS" -OutFile $checksumPath

    # Match the literal filename, including any SemVer build metadata.
    $entries = @(Get-Content -LiteralPath $checksumPath | Where-Object {
      $fields = $_ -split '  ', 2
      $fields.Count -eq 2 -and $fields[1] -ceq $archive
    })
    if ($entries.Count -ne 1 -or $entries[0] -cnotmatch '^[0-9a-fA-F]{64}  ') {
      throw "expected exactly one valid checksum for $archive in SHA256SUMS"
    }
    $expected = $entries[0].Substring(0, 64).ToLowerInvariant()
    $actual = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($expected -ne $actual) {
      throw "checksum verification failed for $archive (expected $expected, got $actual)"
    }

    Expand-Archive -LiteralPath $archivePath -DestinationPath $workdir -Force
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    $payload = Join-Path $workdir "gat-$tag-$target\gat.exe"
    $payloadItem = Get-Item -LiteralPath $payload -Force
    if ($payloadItem.PSIsContainer -or ($payloadItem.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
      throw "archive does not contain a regular gat executable"
    }
    $stagedPath = Join-Path $InstallDir (".gat-install." + [Guid]::NewGuid().ToString('N') + ".exe")
    Copy-Item -LiteralPath $payload -Destination $stagedPath
    # A child shell waits for admission to a kill-on-close job before it can
    # start gat. This avoids the race between launching a probe and assigning
    # its job, and works on both Windows PowerShell 5.1 and PowerShell 7.
    if (-not ('GatInstallerProbeJob' -as [type])) {
      $jobSource = @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
public sealed class GatInstallerProbeJob : IDisposable {
  [StructLayout(LayoutKind.Sequential)]
  struct BasicLimits {
    public long ProcessTime, JobTime;
    public uint Flags;
    public UIntPtr MinimumWorkingSet, MaximumWorkingSet;
    public uint ActiveProcesses;
    public UIntPtr Affinity;
    public uint Priority, Scheduling;
  }
  [StructLayout(LayoutKind.Sequential)]
  struct ExtendedLimits {
    public BasicLimits Basic;
    public ulong ReadOperations, WriteOperations, OtherOperations;
    public ulong ReadBytes, WriteBytes, OtherBytes;
    public UIntPtr ProcessMemory, JobMemory, PeakProcessMemory, PeakJobMemory;
  }
  [DllImport("kernel32.dll", SetLastError = true)]
  static extern IntPtr CreateJobObject(IntPtr attributes, string name);
  [DllImport("kernel32.dll", SetLastError = true)]
  static extern bool SetInformationJobObject(IntPtr job, int infoClass, ref ExtendedLimits info, uint size);
  [DllImport("kernel32.dll", SetLastError = true)]
  static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);
  [StructLayout(LayoutKind.Sequential)]
  struct Accounting {
    public long UserTime, KernelTime, PeriodUserTime, PeriodKernelTime;
    public uint PageFaults, TotalProcesses, ActiveProcesses, TerminatedProcesses;
  }
  [DllImport("kernel32.dll", SetLastError = true)]
  static extern bool QueryInformationJobObject(IntPtr job, int infoClass, out Accounting info, uint size, IntPtr length);
  [DllImport("kernel32.dll", SetLastError = true)]
  static extern bool TerminateJobObject(IntPtr job, uint exitCode);
  [DllImport("kernel32.dll")]
  static extern bool CloseHandle(IntPtr handle);
  IntPtr handle;
  public GatInstallerProbeJob() {
    handle = CreateJobObject(IntPtr.Zero, null);
    if (handle == IntPtr.Zero) throw new Win32Exception();
    var limits = new ExtendedLimits();
    limits.Basic.Flags = 0x2000; // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
    if (!SetInformationJobObject(handle, 9, ref limits, (uint)Marshal.SizeOf(limits))) {
      var error = new Win32Exception(); CloseHandle(handle); handle = IntPtr.Zero; throw error;
    }
  }
  public void Assign(IntPtr process) {
    if (!AssignProcessToJobObject(handle, process)) throw new Win32Exception();
  }
  public void Dispose() {
    if (handle == IntPtr.Zero) return;
    try {
      // Wait for termination before deleting or replacing the staged executable.
      if (!TerminateJobObject(handle, 1)) throw new Win32Exception();
      var deadline = System.Diagnostics.Stopwatch.StartNew();
      Accounting info;
      do {
        if (!QueryInformationJobObject(handle, 1, out info, (uint)Marshal.SizeOf(typeof(Accounting)), IntPtr.Zero))
          throw new Win32Exception();
        if (info.ActiveProcesses == 0) return;
        if (deadline.ElapsedMilliseconds >= 1000) throw new TimeoutException("probe job cleanup timed out");
        System.Threading.Thread.Sleep(10);
      } while (true);
    } finally { CloseHandle(handle); handle = IntPtr.Zero; }
  }
}
'@
      $compileParameters = $null
      try {
        if ($PSVersionTable.PSEdition -eq 'Desktop') {
          $compileParameters = New-Object CodeDom.Compiler.CompilerParameters
          $compileParameters.GenerateInMemory = $true
          $null = $compileParameters.ReferencedAssemblies.Add('System.dll')
          $compileParameters.TempFiles = New-Object CodeDom.Compiler.TempFileCollection($workdir, $false)
          Add-Type -TypeDefinition $jobSource -CompilerParameters $compileParameters
        } else {
          Add-Type -TypeDefinition $jobSource
        }
      } finally {
        if ($null -ne $compileParameters) { $compileParameters.TempFiles.Dispose() }
      }
    }
    $probe = [Diagnostics.Process]::new()
    $shellName = if ($PSVersionTable.PSEdition -eq 'Core') { 'pwsh.exe' } else { 'powershell.exe' }
    $probe.StartInfo.FileName = Join-Path $PSHOME $shellName
    $command = "if (`$null -eq [Console]::ReadLine()) { exit 1 }; " +
      "`$p = New-Object Diagnostics.Process; `$p.StartInfo.FileName = '" + $stagedPath.Replace("'", "''") +
      "'; `$p.StartInfo.Arguments = '--version'; `$p.StartInfo.UseShellExecute = `$false; " +
      "`$null = `$p.Start(); `$p.WaitForExit(); exit `$p.ExitCode"
    $encodedCommand = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($command))
    $probe.StartInfo.Arguments = '-NoLogo -NoProfile -NonInteractive -EncodedCommand ' + $encodedCommand
    $probe.StartInfo.UseShellExecute = $false
    $probe.StartInfo.CreateNoWindow = $true
    $probe.StartInfo.RedirectStandardInput = $true
    $probe.StartInfo.RedirectStandardOutput = $true
    $probe.StartInfo.RedirectStandardError = $true
    $job = [GatInstallerProbeJob]::new()
    $probeStarted = $false
    try {
      $deadline = [Diagnostics.Stopwatch]::StartNew()
      $probeStarted = $probe.Start()
      if (-not $probeStarted) { throw "could not start downloaded gat; existing installation preserved" }
      $job.Assign($probe.Handle)
      $probe.StandardInput.WriteLine('start')
      $probe.StandardInput.Close()
      # Drain both pipes asynchronously so neither can block process exit.
      $stdout = $probe.StandardOutput.ReadToEndAsync()
      $stderr = $probe.StandardError.ReadToEndAsync()
      if (-not $probe.WaitForExit([Math]::Max(0, 15000 - [int]$deadline.ElapsedMilliseconds)) -or
          -not [Threading.Tasks.Task]::WaitAll([Threading.Tasks.Task[]]@($stdout, $stderr),
            [Math]::Max(0, 15000 - [int]$deadline.ElapsedMilliseconds))) {
        throw "gat startup timed out; existing installation preserved"
      }
      $reportedVersion = $stdout.GetAwaiter().GetResult().TrimEnd([char[]]"`r`n")
      $null = $stderr.GetAwaiter().GetResult()
      if ($probe.ExitCode -ne 0) {
        throw "downloaded gat cannot run on this system; existing installation preserved"
      }
      if ($reportedVersion -cne "gat $core") {
        throw "downloaded gat reported an unexpected version; existing installation preserved"
      }
    } finally {
      try { $job.Dispose() } finally {
        if ($probeStarted -and -not $probe.HasExited) {
          $probe.Kill()
          $null = $probe.WaitForExit(1000)
        }
        if ($probeStarted) {
          $probe.StandardOutput.Dispose()
          $probe.StandardError.Dispose()
        }
        $probe.Dispose()
      }
    }
    Assert-InstallDestination $destination
    # Replace on the destination filesystem without first deleting a locked executable.
    if ([IO.File]::Exists($destination)) {
      [IO.File]::Replace($stagedPath, $destination, [NullString]::Value)
    } else {
      [IO.File]::Move($stagedPath, $destination)
    }

    Write-Information -InformationAction Continue "Installed gat $tag to $InstallDir\gat.exe"
    $pathEntries = ($env:Path -split ";") | Where-Object { $_ -ne "" }
    if (-not ($pathEntries -contains $InstallDir)) {
      Write-Information -InformationAction Continue "Note: $InstallDir is not on your PATH."
      Write-Information -InformationAction Continue "  Add it, e.g.: `$env:Path = `"$InstallDir;`$env:Path`""
    }
  } finally {
    try {
      if ($stagedPath) {
        # Windows can briefly retain image-file locks after a job reports that
        # its processes have exited. Bound cleanup retries to that specific case.
        $cleanupDeadline = [Diagnostics.Stopwatch]::StartNew()
        while ([IO.File]::Exists($stagedPath)) {
          try {
            # A failed copy may retain source attributes. Clear them only on
            # our staged file so read-only payloads cannot defeat cleanup.
            [IO.File]::SetAttributes($stagedPath, [IO.FileAttributes]::Normal)
            [IO.File]::Delete($stagedPath)
            break
          } catch [IO.IOException], [UnauthorizedAccessException] {
            if ($cleanupDeadline.ElapsedMilliseconds -ge 1000) { throw }
            [Threading.Thread]::Sleep(25)
          }
        }
      }
    } finally {
      Remove-Item -LiteralPath $workdir -Recurse -Force -ErrorAction SilentlyContinue
    }
  }
} -Version $Version -VersionSpecified ($PSBoundParameters.ContainsKey('Version'))
