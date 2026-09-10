# Exercise the real transport with a local HTTP peer, including bodies that
# never finish. These tests also run on Windows PowerShell 5.1.
$ErrorActionPreference = 'Stop'
$tokens = $null
$parseErrors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile(
  (Join-Path $PSScriptRoot '../../docs/install.ps1'), [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count) { throw $parseErrors[0] }
foreach ($name in @('Resolve-ReleaseRedirect', 'Invoke-ReleaseRequest', 'Get-ReleaseResource')) {
  $helper = $ast.Find({ param($node)
    $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name
  }, $true)
  . ([scriptblock]::Create($helper.Extent.Text))
}

Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Net;
using System.Net.Sockets;
using System.Text;
using System.Threading;
public sealed class InstallerHttpPeer : IDisposable {
  readonly TcpListener listener = new TcpListener(IPAddress.Loopback, 0);
  readonly ManualResetEvent stop = new ManualResetEvent(false);
  readonly Thread thread;
  public bool BodyStarted;
  public int Requests;
  public string Uri { get; private set; }
  public InstallerHttpPeer(string mode) {
    listener.Start();
    Uri = "http://127.0.0.1:" + ((IPEndPoint)listener.LocalEndpoint).Port + "/";
    thread = new Thread(() => Serve(mode));
    thread.IsBackground = true;
    thread.Start();
  }
  void Serve(string mode) {
    try {
      while (!stop.WaitOne(0)) {
      using (var client = listener.AcceptTcpClient())
      using (var stream = client.GetStream()) {
        Requests++;
        // Read headers before closing a deliberately incomplete response.
        var reader = new StreamReader(stream);
        while (!String.IsNullOrEmpty(reader.ReadLine())) { }
        if (mode == "redirect_loop" || mode == "redirect_missing" ||
            (mode == "redirect" && Requests == 1)) {
          string location = mode == "redirect_missing" ? "" : "Location: /target\r\n";
          byte[] redirect = Encoding.ASCII.GetBytes("HTTP/1.1 302 Found\r\n" + location +
            "Content-Length: 1000000\r\nConnection: close\r\n\r\n");
          stream.Write(redirect, 0, redirect.Length);
          continue; // Deliberately omit the body; only headers matter here.
        }
        bool truncate = mode == "reset_always" || (mode == "reset_once" && Requests == 1);
        if (mode == "headers") { stop.WaitOne(); return; }
        byte[] data = Encoding.ASCII.GetBytes("{\"tag_name\":\"v0.1.0\"}");
        int size = (mode == "body" || mode == "trickle" || truncate) ? 1000000 : data.Length;
        string status = mode == "missing" ? "404 Not Found" : "200 OK";
        byte[] headers = Encoding.ASCII.GetBytes("HTTP/1.1 " + status + "\r\nContent-Length: " + size + "\r\nConnection: close\r\n\r\n");
        stream.Write(headers, 0, headers.Length);
        BodyStarted = true;
        if (truncate) { stream.WriteByte(32); continue; }
        if (mode == "body") { stop.WaitOne(); return; }
        if (mode == "trickle") {
          while (!stop.WaitOne(10)) stream.WriteByte(32);
        } else {
          stream.Write(data, 0, data.Length);
          stop.WaitOne();
        }
      }
      }
    } catch (IOException) { } catch (SocketException) { } catch (ObjectDisposedException) { }
  }
  public void Dispose() {
    stop.Set();
    listener.Stop();
    thread.Join();
    stop.Dispose();
  }
}
'@

foreach ($mode in @('json', 'file', 'missing', 'headers', 'body', 'trickle', 'redirect', 'redirect_loop', 'redirect_missing')) {
  $peer = [InstallerHttpPeer]::new($mode)
  $outputPath = Join-Path ([IO.Path]::GetTempPath()) ([IO.Path]::GetRandomFileName())
  try {
    $arguments = @{ Uri = $peer.Uri; TimeoutMilliseconds = 2000 }
    if ($mode -eq 'file') { $arguments.OutFile = $outputPath }
    $failure = $null
    try { $result = Invoke-ReleaseRequest @arguments } catch { $failure = $_ }
    if ($mode -in @('headers', 'body', 'trickle')) {
      if ($null -eq $failure -or $failure.Exception.Message -notmatch 'download timed out') {
        throw "deadline did not stop $mode response: $failure"
      }
      if ($mode -ne 'headers' -and -not $peer.BodyStarted) { throw 'body deadline was not exercised' }
    } elseif ($mode -eq 'redirect_loop') {
      if ($null -eq $failure -or $failure.Exception.Message -notmatch 'too many release redirects' -or
          $peer.Requests -ne 11) { throw 'redirect limit was not enforced' }
    } elseif ($mode -eq 'redirect_missing') {
      if ($null -eq $failure -or $failure.Exception.Message -notmatch 'redirect has no location') {
        throw 'redirect without a location was accepted'
      }
    } elseif ($mode -eq 'missing') {
      if ($null -eq $failure) { throw 'HTTP failure was accepted' }
      $cause = $failure.Exception
      while ($null -ne $cause.InnerException) { $cause = $cause.InnerException }
      if ($cause -isnot [Net.WebException] -or [int]$cause.Response.StatusCode -ne 404) {
        throw 'HTTP status was lost'
      }
    } else {
      if ($null -ne $failure) { throw $failure }
      if ($mode -eq 'file') { $result = [IO.File]::ReadAllText($outputPath) | ConvertFrom-Json }
      if ($mode -eq 'redirect' -and $peer.Requests -ne 2) { throw 'redirect was not followed' }
      if ($result.tag_name -cne 'v0.1.0') { throw 'response was not fully downloaded' }
    }
    Write-Output "PASS: HTTP $mode"
  } finally {
    $peer.Dispose()
    Remove-Item -LiteralPath $outputPath -Force -ErrorAction SilentlyContinue
  }
}

# Exercise retries through the real transport, including truncation of a
# partially written file. Local file failures must not be retried.
foreach ($mode in @('reset_once', 'reset_always', 'disk_failure')) {
  $peer = [InstallerHttpPeer]::new($mode)
  $outputPath = Join-Path ([IO.Path]::GetTempPath()) ([IO.Path]::GetRandomFileName())
  try {
    if ($mode -eq 'disk_failure') { [IO.Directory]::CreateDirectory($outputPath) | Out-Null }
    $failure = $null
    try { Get-ReleaseResource -Uri $peer.Uri -OutFile $outputPath } catch { $failure = $_ }
    if ($mode -eq 'reset_once') {
      if ($null -ne $failure) { throw $failure }
      $result = [IO.File]::ReadAllText($outputPath) | ConvertFrom-Json
      if ($result.tag_name -cne 'v0.1.0' -or $peer.Requests -ne 2) { throw 'body retry did not recover' }
    } elseif ($mode -eq 'reset_always') {
      if ($null -eq $failure -or $peer.Requests -ne 3) { throw 'body retry budget was not enforced' }
    } elseif ($null -eq $failure -or $peer.Requests -ne 1) {
      throw 'local file failure was retried'
    }
    Write-Output "PASS: HTTP $mode"
  } finally {
    $peer.Dispose()
    Remove-Item -LiteralPath $outputPath -Force -Recurse -ErrorAction SilentlyContinue
  }
}

# Check scheme policy independently of certificate stores and TLS backends.
foreach ($location in @('http://example.invalid/payload', 'file:///payload', 'ftp://example.invalid/payload')) {
  $failure = $null
  try { $null = Resolve-ReleaseRedirect -Source 'https://example.invalid/start' -Location $location } catch { $failure = $_ }
  if ($null -eq $failure -or $failure.Exception.Message -notmatch 'weaken or change the transport') {
    throw "unsafe redirect accepted: $location"
  }
  Write-Output "PASS: reject redirect $location"
}
$target = Resolve-ReleaseRedirect -Source 'https://example.invalid/start' -Location '//assets.example.invalid/payload'
if ($target.AbsoluteUri -cne 'https://assets.example.invalid/payload') { throw 'secure asset redirect rejected' }
Write-Output 'PASS: preserve HTTPS across hosts'
