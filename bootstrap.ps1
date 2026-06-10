param(
	[ValidateSet('Debug', 'Release')]
	[string]$config = 'Release',

	[switch]$clean,
	[switch]$no_pause
)

$ErrorActionPreference = 'Stop'

function get_vs_dev_cmd_path() {
	$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
	if (-not (Test-Path $vswhere))
	{
		throw "vswhere.exe not found: $vswhere"
	}

	$install_path = & $vswhere -latest -products * -requires Microsoft.Component.MSBuild -property installationPath
	if (-not $install_path)
	{
		throw "Visual Studio not found via vswhere."
	}

	$vs_dev_cmd = Join-Path $install_path 'Common7\Tools\VsDevCmd.bat'
	if (-not (Test-Path $vs_dev_cmd))
	{
		throw "VsDevCmd.bat not found: $vs_dev_cmd"
	}

	return $vs_dev_cmd
}

$repo_root = $PSScriptRoot
$deploy_dll_path = 'D:\float\OneDrive\diatom\conf\dll\control\velotype.dll'

# Read target directory from .cargo/config.toml, defaulting to target/
$target_dir = Join-Path $repo_root 'target'
$cargo_config_path = Join-Path $repo_root '.cargo\config.toml'
if (Test-Path $cargo_config_path)
{
	$config_content = Get-Content -Raw -LiteralPath $cargo_config_path
	if ($config_content -match 'target-dir\s*=\s*"([^"]+)"')
	{
		$target_dir = $Matches[1]
	}
}

$profile_dir = $config.ToLower()
$built_dll_path = Join-Path (Join-Path $target_dir $profile_dir) 'velotype.dll'

Write-Host "== Cargo Build ($config) =="
$vs_dev_cmd = get_vs_dev_cmd_path

$cargo_args = "build --lib"
if ($config -eq 'Release')
{
	$cargo_args += " --release"
}

$commands = @(
	"`"$vs_dev_cmd`" -arch=amd64 -host_arch=amd64 -no_logo"
)

if ($clean)
{
	$commands += "cargo clean"
}

$commands += "cargo $cargo_args"
$cmd_string = $commands -join " && "

cmd.exe /c $cmd_string
if ($LASTEXITCODE -ne 0)
{
	Write-Host ""
	Write-Host "Build failed (exit code $LASTEXITCODE). Skipping deploy."
	Write-Host ""
	if (-not $no_pause)
	{
		Read-Host -Prompt "Done. Press Enter to close" | Out-Null
	}
	return
}

if (-not (Test-Path -LiteralPath $built_dll_path))
{
	throw "Build succeeded but velotype.dll not found: $built_dll_path"
}

# Copy to local target folder to support test scripts
$local_dll_out_dir = Join-Path (Join-Path $repo_root 'target') $profile_dir
New-Item -ItemType Directory -Force -Path $local_dll_out_dir | Out-Null
$local_dll_out_path = Join-Path $local_dll_out_dir 'velotype.dll'

Copy-Item -Force -LiteralPath $built_dll_path -Destination $local_dll_out_path
Write-Host "OK: velotype.dll => $local_dll_out_path"

$built_lib_path = Join-Path (Join-Path $target_dir $profile_dir) 'velotype.dll.lib'
if (Test-Path -LiteralPath $built_lib_path)
{
	$local_lib_out_path = Join-Path $local_dll_out_dir 'velotype.dll.lib'
	Copy-Item -Force -LiteralPath $built_lib_path -Destination $local_lib_out_path
	Write-Host "OK: velotype.dll.lib => $local_lib_out_path"
}

# Deploy to target destination
$deploy_dir = Split-Path -Parent $deploy_dll_path
New-Item -ItemType Directory -Force -Path $deploy_dir | Out-Null
if (Test-Path -LiteralPath $deploy_dll_path)
{
	$ts = Get-Date -Format 'yyyyMMdd_HHmmssfff'
	$bak_name = "__velotype_$ts.dll"
	$bak_path = Join-Path $deploy_dir $bak_name
	Rename-Item -Force -LiteralPath $deploy_dll_path -NewName $bak_name
	Write-Host "OK: backed up deploy dll => $bak_path"
}
Copy-Item -Force -LiteralPath $built_dll_path -Destination $deploy_dll_path
Write-Host "OK: deployed => $deploy_dll_path"

Write-Host ""
if (-not $no_pause)
{
	Read-Host -Prompt "Done. Press Enter to close" | Out-Null
}
