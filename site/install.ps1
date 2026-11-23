# Installs the latest agent release: irm https://agent.rokartur.com/install.ps1 | iex
& {
  $ErrorActionPreference = 'Stop'
  $release = 'https://github.com/rokartur/agent/releases/latest/download'
  $asset = 'agent-windows-x86_64.exe'
  $dir = if ($env:AGENT_INSTALL_DIR) { $env:AGENT_INSTALL_DIR } else { "$env:LOCALAPPDATA\Programs\agent" }
  New-Item -ItemType Directory -Force -Path $dir | Out-Null
  $staged = Join-Path $dir ".agent-install-$PID.exe"

  try {
    Invoke-WebRequest -UseBasicParsing "$release/$asset" -OutFile $staged
    $sums = (Invoke-WebRequest -UseBasicParsing "$release/SHA256SUMS").Content
    if ($sums -is [byte[]]) { $sums = [Text.Encoding]::UTF8.GetString($sums) }
    $want = ''
    foreach ($line in $sums -split "`n") {
      $fields = $line.Trim() -split '\s+'
      if ($fields.Count -eq 2 -and $fields[1] -eq $asset) { $want = $fields[0] }
    }
    $got = (Get-FileHash -Algorithm SHA256 $staged).Hash
    if (-not $want -or $want -ne $got) { throw 'agent: the download does not match its published SHA-256.' }
    Move-Item -Force $staged (Join-Path $dir 'agent.exe')
  } finally {
    Remove-Item -Force -ErrorAction SilentlyContinue $staged
  }
  Write-Host "Installed $(& (Join-Path $dir 'agent.exe') --version) to $dir\agent.exe"

  # Read the raw value so %VARS% in the user PATH stay unexpanded when written back.
  $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
  $userPath = $key.GetValue('Path', '', 'DoNotExpandEnvironmentNames').TrimEnd(';')
  if (($userPath -split ';') -notcontains $dir) {
    $newPath = if ($userPath) { "$userPath;$dir" } else { $dir }
    $key.SetValue('Path', $newPath, 'ExpandString')
    # A User-scope set broadcasts WM_SETTINGCHANGE, so new terminals see the new PATH.
    [Environment]::SetEnvironmentVariable('AGENT_PATH_REFRESH', $null, 'User')
    Write-Host "Added $dir to your PATH; open a new terminal to use agent."
  }
}
