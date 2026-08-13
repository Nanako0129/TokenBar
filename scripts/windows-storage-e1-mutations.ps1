$ErrorActionPreference = 'Continue'
$Source = Join-Path $PSScriptRoot 'crates/tb_core_ffi/src/agent_storage_windows.rs'
$Original = [System.IO.File]::ReadAllText($Source)
$LineEnding = if ($Original.Contains("`r`n")) { "`r`n" } else { "`n" }
Set-Location $PSScriptRoot

function Replace-Once([string]$Text, [string]$Old, [string]$New, [string]$Label) {
    $Index = $Text.IndexOf($Old, [System.StringComparison]::Ordinal)
    if ($Index -lt 0) { throw "$Label pattern missing" }
    if ($Text.IndexOf($Old, $Index + $Old.Length, [System.StringComparison]::Ordinal) -ge 0) {
        throw "$Label pattern not unique"
    }
    $Text.Substring(0, $Index) + $New + $Text.Substring($Index + $Old.Length)
}

function Run-Test([string]$Name) {
    & cargo test -p tb_core_ffi --release --locked $Name -- --exact --nocapture 2>&1 | Out-Host
    $Code = $LASTEXITCODE
    return $Code
}

$Cases = @(
    @{
        Label = 'M-ACL'
        Tests = @('agent_storage_windows::tests::acl_descriptor_and_handle_mutations_fail_closed')
        Edit = {
            param($Text)
            $Pattern = 'if ace.ace_type != ACCESS_ALLOWED_ACE_TYPE_VALUE' + $LineEnding +
                '            || ace.flags != NO_INHERITANCE as u8' + $LineEnding +
                '            || ace.mask != FILE_ALL_ACCESS'
            Replace-Once $Text $Pattern 'if false' 'M-ACL'
        }
    },
    @{
        Label = 'M-SHARE'
        Tests = @(
            'agent_storage_windows::tests::secure_lock_preserves_one_identity_and_blocks_delete_until_handles_drop',
            'agent_storage_windows::tests::validation_handle_blocks_replacement_until_identity_check_finishes'
        )
        Edit = {
            param($Text)
            Replace-Once $Text 'const VALIDATION_SHARE_MODE: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE;' 'const VALIDATION_SHARE_MODE: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;' 'M-SHARE'
        }
    },
    @{
        Label = 'M-IDENTITY'
        Tests = @(
            'agent_storage_windows::tests::injected_post_replace_identity_mismatch_is_detected',
            'agent_storage_windows::tests::path_replacement_is_detected_without_rewriting_either_file'
        )
        Edit = {
            param($Text)
            $Text = Replace-Once $Text 'if installed_identity != staged_identity {' 'if false && installed_identity != staged_identity {' 'M-IDENTITY/install'
            Replace-Once $Text '    verify_path_identity(path, StorageObjectKind::RegularFile, identity)' '    Ok(())' 'M-IDENTITY/path'
        }
    },
    @{
        Label = 'M-ROLLBACK'
        Tests = @('agent_storage_windows::tests::quarantine_fault_phases_preserve_commit_boundaries')
        Edit = {
            param($Text)
            $Pattern = '    let candidate = open_secure_file_with_identity(candidate_path, expected)?;' + $LineEnding +
                '    verify_secure_file_path(source, source_path)?;' + $LineEnding +
                '    drop(candidate);'
            Replace-Once $Text $Pattern '    verify_secure_file_path(source, source_path)?;' 'M-ROLLBACK'
        }
    },
    @{
        Label = 'M-PRIVACY'
        Tests = @('agent_storage_windows::tests::security_errors_do_not_disclose_path_sid_or_username')
        Edit = {
            param($Text)
            Replace-Once $Text '        "Windows storage security verification failed",' '        "Windows storage security verification failed: raw-secret-sentinel",' 'M-PRIVACY'
        }
    }
)

$Survived = $false
try {
    Write-Output ('HEAD=' + (git rev-parse HEAD))
    Write-Output ('BASE_BLOB=' + (git hash-object $Source))
    foreach ($Case in $Cases) {
        foreach ($Test in $Case.Tests) {
            if ((Run-Test $Test) -ne 0) { throw "baseline failed: $Test" }
        }
    }
    foreach ($Case in $Cases) {
        $Label = $Case.Label
        [System.IO.File]::WriteAllText($Source, (& $Case.Edit $Original))
        $Killed = $false
        foreach ($Test in $Case.Tests) {
            $Code = Run-Test $Test
            Write-Output "MUTANT_RESULT label=$Label test=$Test exit=$Code"
            if ($Code -ne 0) { $Killed = $true }
        }
        if ($Killed) { Write-Output "MUTATION_KILLED label=$Label" }
        else { Write-Output "MUTATION_SURVIVED label=$Label"; $Survived = $true }
        [System.IO.File]::WriteAllText($Source, $Original)
        foreach ($Test in $Case.Tests) {
            if ((Run-Test $Test) -ne 0) { throw "restore failed: $Label / $Test" }
        }
    }
} finally {
    [System.IO.File]::WriteAllText($Source, $Original)
}
Write-Output ('FINAL_BLOB=' + (git hash-object $Source))
if ($Survived) { exit 1 }
