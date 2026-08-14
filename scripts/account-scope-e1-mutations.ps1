$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$AccountSource = Join-Path $PSScriptRoot 'crates/tb_core_ffi/src/agent_account_scope.rs'
$StorageSource = Join-Path $PSScriptRoot 'crates/tb_core_ffi/src/agent_storage_windows.rs'
$Utf8NoBom = [System.Text.UTF8Encoding]::new($false)
$OriginalAccountBytes = [System.IO.File]::ReadAllBytes($AccountSource)
$OriginalStorageBytes = [System.IO.File]::ReadAllBytes($StorageSource)
$OriginalAccount = $Utf8NoBom.GetString($OriginalAccountBytes)
$OriginalStorage = $Utf8NoBom.GetString($OriginalStorageBytes)
$AccountLineEnding = if ($OriginalAccount.Contains("`r`n")) { "`r`n" } else { "`n" }
$StorageLineEnding = if ($OriginalStorage.Contains("`r`n")) { "`r`n" } else { "`n" }
$AccountMarker = '#[cfg(test)]' + $AccountLineEnding + 'pub(crate) mod test_support {'
$StorageMarker = '#[cfg(test)]' + $StorageLineEnding + 'mod tests {'
$TestTimeoutMilliseconds = 30 * 60 * 1000
$RunnerStatus = '?? account-scope-e1-mutations.ps1'

$CredentialVariables = @(
    'ANTHROPIC_API_KEY',
    'CLAUDE_API_KEY',
    'CLAUDE_CODE_OAUTH_TOKEN',
    'OPENAI_API_KEY',
    'CODEX_API_KEY',
    'GOOGLE_API_KEY',
    'GEMINI_API_KEY',
    'GOOGLE_APPLICATION_CREDENTIALS',
    'GROK_API_KEY',
    'XAI_API_KEY',
    'ANTIGRAVITY_OAUTH_CLIENT_ID',
    'ANTIGRAVITY_OAUTH_CLIENT_SECRET',
    'AWS_ACCESS_KEY_ID',
    'AWS_SECRET_ACCESS_KEY',
    'AWS_SESSION_TOKEN',
    'AWS_WEB_IDENTITY_TOKEN_FILE',
    'AZURE_OPENAI_API_KEY',
    'AZURE_OPENAI_ENDPOINT',
    'DEEPSEEK_API_KEY',
    'OPENROUTER_API_KEY',
    'MOONSHOT_API_KEY',
    'MINIMAX_API_KEY',
    'ZHIPUAI_API_KEY',
    'DASHSCOPE_API_KEY',
    'MISTRAL_API_KEY',
    'COHERE_API_KEY',
    'TOGETHER_API_KEY',
    'PERPLEXITY_API_KEY',
    'TOKENBAR_CLAUDE_OAUTH_SCOPES',
    'TOKCAT_CLAUDE_OAUTH_SCOPES',
    'CODEXBAR_CLAUDE_OAUTH_SCOPES'
)
foreach ($Name in $CredentialVariables) {
    [System.Environment]::SetEnvironmentVariable($Name, $null, 'Process')
}
Set-Location $PSScriptRoot

function Replace-Once(
    [string]$Text,
    [string]$Old,
    [string]$New,
    [string]$Label
) {
    $Index = $Text.IndexOf($Old, [System.StringComparison]::Ordinal)
    if ($Index -lt 0) { throw "$Label pattern missing" }
    if ($Text.IndexOf($Old, $Index + $Old.Length, [System.StringComparison]::Ordinal) -ge 0) {
        throw "$Label pattern not unique"
    }
    $Replaced = $Text.Substring(0, $Index) + $New + $Text.Substring($Index + $Old.Length)
    if ($Replaced -ceq $Text) { throw "$Label replacement made no change" }
    return $Replaced
}

function Invoke-GitValue([string[]]$Arguments) {
    $Lines = @(& git @Arguments 2>$null)
    if ($LASTEXITCODE -ne 0 -or $Lines.Count -ne 1) { throw 'Git query failed' }
    return ("$($Lines[0])").Trim()
}

function Get-Sha256Bytes([byte[]]$Bytes) {
    $Hasher = [System.Security.Cryptography.SHA256]::Create()
    try {
        return [System.Convert]::ToHexString($Hasher.ComputeHash($Bytes)).ToLowerInvariant()
    } finally {
        $Hasher.Dispose()
    }
}

function Get-Sha256Text([string]$Text) {
    return (Get-Sha256Bytes ($Utf8NoBom.GetBytes($Text)))
}

function Find-UniqueByteMarker([byte[]]$Bytes, [byte[]]$Marker) {
    $MatchIndex = -1
    $MatchCount = 0
    for ($Index = 0; $Index -le $Bytes.Length - $Marker.Length; $Index++) {
        $Same = $true
        for ($MarkerIndex = 0; $MarkerIndex -lt $Marker.Length; $MarkerIndex++) {
            if ($Bytes[$Index + $MarkerIndex] -ne $Marker[$MarkerIndex]) {
                $Same = $false
                break
            }
        }
        if ($Same) {
            $MatchIndex = $Index
            $MatchCount++
        }
    }
    if ($MatchCount -ne 1) { throw 'Production marker is not unique' }
    return $MatchIndex
}

function Get-ProductionPrefixHash([string]$Source) {
    $Bytes = [System.IO.File]::ReadAllBytes($Source)
    $Marker = [System.Text.Encoding]::UTF8.GetBytes("#[cfg(test)]`npub(crate) mod test_support {")
    $MatchIndex = Find-UniqueByteMarker $Bytes $Marker
    $Prefix = [byte[]]::new($MatchIndex)
    [System.Array]::Copy($Bytes, 0, $Prefix, 0, $MatchIndex)
    return (Get-Sha256Bytes $Prefix)
}

function Get-UniqueSuffix([string]$Text, [string]$Marker) {
    $Index = $Text.IndexOf($Marker, [System.StringComparison]::Ordinal)
    if ($Index -lt 0) { throw 'Test marker is missing' }
    if ($Text.IndexOf($Marker, $Index + $Marker.Length, [System.StringComparison]::Ordinal) -ge 0) {
        throw 'Test marker is not unique'
    }
    return $Text.Substring($Index)
}

function Assert-ProductionOnlyMutation([string]$Original, [string]$Mutated, [string]$Marker) {
    if ((Get-UniqueSuffix $Original $Marker) -cne (Get-UniqueSuffix $Mutated $Marker)) {
        throw 'Mutation changed test bytes'
    }
}

function Write-SourceText([string]$Path, [string]$Text) {
    [System.IO.File]::WriteAllText($Path, $Text, $Utf8NoBom)
}

function Restore-ExactSources {
    [System.IO.File]::WriteAllBytes($AccountSource, $OriginalAccountBytes)
    [System.IO.File]::WriteAllBytes($StorageSource, $OriginalStorageBytes)
}

function Get-WorkingBlobs {
    return [pscustomobject]@{
        Account = (Invoke-GitValue -Arguments @('hash-object', 'crates/tb_core_ffi/src/agent_account_scope.rs'))
        Storage = (Invoke-GitValue -Arguments @('hash-object', 'crates/tb_core_ffi/src/agent_storage_windows.rs'))
    }
}

function Get-SuperprojectStatus {
    $Status = @(& git status --porcelain=v1 --untracked-files=all 2>$null)
    if ($LASTEXITCODE -ne 0) { throw 'Superproject status query failed' }
    return $Status
}

function Get-VendorStatus {
    $Status = @(& git -C vendor/tokscale-core status --porcelain=v1 --untracked-files=all 2>$null)
    if ($LASTEXITCODE -ne 0) { throw 'Vendor status query failed' }
    return $Status
}

function Assert-RunnerOnlyStatus {
    $Status = @(Get-SuperprojectStatus)
    if ($Status.Count -ne 1 -or "$($Status[0])" -cne $RunnerStatus) {
        throw 'Unexpected candidate status'
    }
    if (@(Get-VendorStatus).Count -ne 0) { throw 'Vendor checkout is dirty' }
}

function Assert-MutationStatus([bool]$AccountChanged, [bool]$StorageChanged) {
    $Expected = [System.Collections.Generic.List[string]]::new()
    [void]$Expected.Add($RunnerStatus)
    if ($AccountChanged) { [void]$Expected.Add(' M crates/tb_core_ffi/src/agent_account_scope.rs') }
    if ($StorageChanged) { [void]$Expected.Add(' M crates/tb_core_ffi/src/agent_storage_windows.rs') }
    $Actual = @(Get-SuperprojectStatus | Sort-Object)
    $ExpectedSorted = @($Expected | Sort-Object)
    if ($Actual.Count -ne $ExpectedSorted.Count) { throw 'Unexpected mutation status count' }
    for ($Index = 0; $Index -lt $Actual.Count; $Index++) {
        if ("$($Actual[$Index])" -cne "$($ExpectedSorted[$Index])") {
            throw 'Unexpected mutation status'
        }
    }
    if (@(Get-VendorStatus).Count -ne 0) { throw 'Vendor checkout is dirty' }
}

function Assert-ExactSources {
    $Blobs = Get-WorkingBlobs
    if ($Blobs.Account -cne $env:CANDIDATE_ACCOUNT_BLOB) { throw 'Account source restore mismatch' }
    if ($Blobs.Storage -cne $env:CANDIDATE_STORAGE_BLOB) { throw 'Storage source restore mismatch' }
}

function Invoke-ExactTest([string]$Name) {
    $StartInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $StartInfo.FileName = 'cargo'
    $StartInfo.WorkingDirectory = $PSScriptRoot
    $StartInfo.UseShellExecute = $false
    $StartInfo.RedirectStandardOutput = $true
    $StartInfo.RedirectStandardError = $true
    $StartInfo.CreateNoWindow = $true
    foreach ($Argument in @(
        'test',
        '-p',
        'tb_core_ffi',
        '--release',
        '--locked',
        $Name,
        '--',
        '--exact',
        '--nocapture',
        '--test-threads=1'
    )) {
        [void]$StartInfo.ArgumentList.Add($Argument)
    }
    $StartInfo.Environment['CARGO_TERM_COLOR'] = 'never'
    $StartInfo.Environment['NO_COLOR'] = '1'
    $StartInfo.Environment['RUST_BACKTRACE'] = '0'
    foreach ($Variable in $CredentialVariables) {
        $StartInfo.Environment[$Variable] = ''
    }

    $Process = [System.Diagnostics.Process]::new()
    $Process.StartInfo = $StartInfo
    try {
        if (-not $Process.Start()) { throw 'Cargo test process did not start' }
        $StandardOutput = $Process.StandardOutput.ReadToEndAsync()
        $StandardError = $Process.StandardError.ReadToEndAsync()
        $TimedOut = -not $Process.WaitForExit($TestTimeoutMilliseconds)
        if ($TimedOut) {
            try { $Process.Kill($true) } catch {}
            if (-not $Process.WaitForExit(10000)) { throw 'Cargo test process did not stop' }
        } else {
            $Process.WaitForExit()
        }
        $Output = $StandardOutput.GetAwaiter().GetResult() + "`n" + $StandardError.GetAwaiter().GetResult()
        $ExitCode = if ($TimedOut) { -1 } else { $Process.ExitCode }
        return [pscustomobject]@{
            ExitCode = $ExitCode
            TimedOut = $TimedOut
            Output = $Output
        }
    } finally {
        $Process.Dispose()
    }
}

function Get-ExecutedTestCount([string]$Output) {
    $Total = 0
    foreach ($Match in [System.Text.RegularExpressions.Regex]::Matches(
        $Output,
        '(?m)^\s*running ([0-9]+) tests?\s*$'
    )) {
        $Total += [int]$Match.Groups[1].Value
    }
    return $Total
}

function Assert-PassingTest($Result) {
    if ($Result.TimedOut -or $Result.ExitCode -ne 0) { throw 'Exact test did not pass' }
    if ((Get-ExecutedTestCount $Result.Output) -ne 1) { throw 'Exact test count mismatch' }
    if ($Result.Output.IndexOf('test result: ok. 1 passed; 0 failed;', [System.StringComparison]::Ordinal) -lt 0) {
        throw 'Exact test pass summary missing'
    }
}

function Assert-KilledTest($Result, [string]$Owner) {
    if ($Result.TimedOut -or $Result.ExitCode -ne 101) { throw 'Mutant did not exit through a Rust test failure' }
    if ((Get-ExecutedTestCount $Result.Output) -ne 1) { throw 'Mutant exact test count mismatch' }
    if ($Result.Output.IndexOf('test result: FAILED. 0 passed; 1 failed;', [System.StringComparison]::Ordinal) -lt 0) {
        throw 'Mutant failed outside the exact test owner'
    }
    if ($Result.Output.IndexOf($Owner, [System.StringComparison]::Ordinal) -lt 0) {
        throw 'Fixed owner label missing'
    }
}

$IdentityEdit = { param([string]$Text) return $Text }
$StickyTest = 'agent_account_scope::tests::windows_consumer_secure_fallback_is_sticky_and_preferred_root_untouched'
$RefusalTest = 'agent_account_scope::tests::windows_consumer_refusal_precommit_and_quarantine_failures_are_typed'
$CngTest = 'agent_storage_windows::tests::cng_success_exact_length_and_injected_failure_return_no_partial'
$PostReplaceIdentityTest = 'agent_storage_windows::tests::injected_post_replace_identity_mismatch_is_detected'
$PathIdentityTest = 'agent_storage_windows::tests::path_replacement_is_detected_without_rewriting_either_file'
$QuarantineRollbackTest = 'agent_storage_windows::tests::quarantine_fault_phases_preserve_commit_boundaries'

$Cases = @(
    [pscustomobject]@{
        Label = 'M20'
        Tests = @([pscustomobject]@{ Name = $StickyTest; Owner = 'windows sticky fallback' })
        EditAccount = {
            param([string]$Text)
            $Pattern = '    #[cfg(target_os = "windows")]' + $AccountLineEnding +
                '    let directory = if backend.uses_windows_secure_storage() {' + $AccountLineEnding +
                '        crate::agent_storage_windows::resolve_secure_storage_directory(&directory)' + $AccountLineEnding +
                '            .map_err(|_| AccountScopeError::StorageUnavailable)?' + $AccountLineEnding +
                '    } else {' + $AccountLineEnding +
                '        directory' + $AccountLineEnding +
                '    };'
            $Replacement = '    #[cfg(target_os = "windows")]' + $AccountLineEnding +
                '    let directory = if false && backend.uses_windows_secure_storage() {' + $AccountLineEnding +
                '        crate::agent_storage_windows::resolve_secure_storage_directory(&directory)' + $AccountLineEnding +
                '            .map_err(|_| AccountScopeError::StorageUnavailable)?' + $AccountLineEnding +
                '    } else if backend.uses_windows_secure_storage() {' + $AccountLineEnding +
                '        let mut disabled_name = directory' + $AccountLineEnding +
                '            .file_name()' + $AccountLineEnding +
                '            .ok_or(AccountScopeError::StorageUnavailable)?' + $AccountLineEnding +
                '            .to_os_string();' + $AccountLineEnding +
                '        disabled_name.push(".disabled");' + $AccountLineEnding +
                '        directory.with_file_name(disabled_name)' + $AccountLineEnding +
                '    } else {' + $AccountLineEnding +
                '        directory' + $AccountLineEnding +
                '    };'
            Replace-Once $Text $Pattern $Replacement 'M20'
        }
        EditStorage = $IdentityEdit
    },
    [pscustomobject]@{
        Label = 'M21'
        Tests = @([pscustomobject]@{ Name = $StickyTest; Owner = 'windows sticky fallback' })
        EditAccount = $IdentityEdit
        EditStorage = {
            param([string]$Text)
            Replace-Once $Text `
                '        Ok(_) => return Ok(fallback),' `
                '        Ok(_) => return Ok(fallback.with_extension("bypass")),' `
                'M21'
        }
    },
    [pscustomobject]@{
        Label = 'M22'
        Tests = @([pscustomobject]@{ Name = $RefusalTest; Owner = 'windows secure backend refusal' })
        EditAccount = {
            param([string]$Text)
            $Pattern = '            Ok(Some(file)) => file,' + $AccountLineEnding +
                '            Ok(None) => return Ok(None),' + $AccountLineEnding +
                '            Err(_) => return Err(AccountScopeError::InvalidInstallationKey),'
            $Replacement = '            Ok(Some(file)) => file,' + $AccountLineEnding +
                '            Ok(None) => return Ok(None),' + $AccountLineEnding +
                '            Err(_) => return Ok(None),'
            Replace-Once $Text $Pattern $Replacement 'M22'
        }
        EditStorage = $IdentityEdit
    },
    [pscustomobject]@{
        Label = 'M-CNG-FAILURE'
        Tests = @([pscustomobject]@{ Name = $CngTest; Owner = 'CNG-INJECTED-FAILURE: failure has no result' })
        EditAccount = $IdentityEdit
        EditStorage = {
            param([string]$Text)
            Replace-Once $Text `
                '    if fill(bytes.as_mut_ptr(), len_u32) < 0 {' `
                '    if fill(bytes.as_mut_ptr(), len_u32) == i32::MIN {' `
                'M-CNG-FAILURE'
        }
    },
    [pscustomobject]@{
        Label = 'M-IDENTITY'
        Tests = @(
            [pscustomobject]@{
                Name = $PostReplaceIdentityTest
                Owner = 'REPLACE-POSTCOMMIT: identity mismatch returned'
            },
            [pscustomobject]@{
                Name = $PathIdentityTest
                Owner = 'PATH-REPLACEMENT: identity swap detected'
            }
        )
        EditAccount = $IdentityEdit
        EditStorage = {
            param([string]$Text)
            $Text = Replace-Once $Text `
                'if installed_identity != staged_identity {' `
                'if false && installed_identity != staged_identity {' `
                'M-IDENTITY/install'
            $Pattern = 'pub(crate) fn verify_secure_file_path(file: &File, path: &Path) -> io::Result<()> {' + $StorageLineEnding +
                '    let handle = file.as_raw_handle() as HANDLE;' + $StorageLineEnding +
                '    let identity = storage_identity(handle, StorageObjectKind::RegularFile)?;' + $StorageLineEnding +
                '    verify_storage_handle(handle)?;' + $StorageLineEnding +
                '    verify_path_identity(path, StorageObjectKind::RegularFile, identity)' + $StorageLineEnding +
                '}'
            $Replacement = 'pub(crate) fn verify_secure_file_path(file: &File, path: &Path) -> io::Result<()> {' + $StorageLineEnding +
                '    let handle = file.as_raw_handle() as HANDLE;' + $StorageLineEnding +
                '    let _identity = storage_identity(handle, StorageObjectKind::RegularFile)?;' + $StorageLineEnding +
                '    verify_storage_handle(handle)?;' + $StorageLineEnding +
                '    Ok(())' + $StorageLineEnding +
                '}'
            Replace-Once $Text $Pattern $Replacement 'M-IDENTITY/path'
        }
    },
    [pscustomobject]@{
        Label = 'M-ROLLBACK'
        Tests = @([pscustomobject]@{
            Name = $QuarantineRollbackTest
            Owner = 'QUARANTINE-SWAP: rollback targeted unrelated candidate'
        })
        EditAccount = $IdentityEdit
        EditStorage = {
            param([string]$Text)
            $Pattern = '    let candidate = open_secure_file_with_identity(candidate_path, expected)?;' + $StorageLineEnding +
                '    verify_secure_file_path(source, source_path)?;' + $StorageLineEnding +
                '    drop(candidate);'
            Replace-Once $Text $Pattern '    verify_secure_file_path(source, source_path)?;' 'M-ROLLBACK'
        }
    }
)

function Get-MutatedState($Case) {
    $AccountEditor = $Case.EditAccount
    $StorageEditor = $Case.EditStorage
    $AccountOutput = @(& $AccountEditor $OriginalAccount)
    $StorageOutput = @(& $StorageEditor $OriginalStorage)
    if ($AccountOutput.Count -ne 1 -or $StorageOutput.Count -ne 1) {
        throw 'Mutation edit returned an invalid result'
    }
    $Account = [string]$AccountOutput[0]
    $Storage = [string]$StorageOutput[0]
    if ($Account -ceq $OriginalAccount -and $Storage -ceq $OriginalStorage) {
        throw 'Mutation made no production change'
    }
    Assert-ProductionOnlyMutation $OriginalAccount $Account $AccountMarker
    Assert-ProductionOnlyMutation $OriginalStorage $Storage $StorageMarker
    return [pscustomobject]@{
        Account = $Account
        Storage = $Storage
        AccountChanged = $Account -cne $OriginalAccount
        StorageChanged = $Storage -cne $OriginalStorage
    }
}

function Write-MutatedState($State) {
    if ($State.AccountChanged) { Write-SourceText $AccountSource $State.Account }
    if ($State.StorageChanged) { Write-SourceText $StorageSource $State.Storage }
}

function Assert-RequiredEnvironment {
    foreach ($Name in @(
        'CANDIDATE_SHA',
        'CANDIDATE_TREE',
        'CANDIDATE_ACCOUNT_BLOB',
        'CANDIDATE_STORAGE_BLOB',
        'CANDIDATE_VERIFICATION_BLOB',
        'CANDIDATE_VENDOR_SHA',
        'CANDIDATE_PRODUCTION_PREFIX_SHA256'
    )) {
        if ([string]::IsNullOrWhiteSpace([System.Environment]::GetEnvironmentVariable($Name))) {
            throw 'Required identity input is missing'
        }
    }
}

function Assert-InitialIdentity {
    Assert-RequiredEnvironment
    $Head = Invoke-GitValue -Arguments @('rev-parse', 'HEAD')
    $Tree = Invoke-GitValue -Arguments @('rev-parse', 'HEAD^{tree}')
    $AccountObject = Invoke-GitValue -Arguments @('rev-parse', ('{0}:{1}' -f $env:CANDIDATE_SHA, 'crates/tb_core_ffi/src/agent_account_scope.rs'))
    $StorageObject = Invoke-GitValue -Arguments @('rev-parse', ('{0}:{1}' -f $env:CANDIDATE_SHA, 'crates/tb_core_ffi/src/agent_storage_windows.rs'))
    $VerificationObject = Invoke-GitValue -Arguments @('rev-parse', ('{0}:{1}' -f $env:CANDIDATE_SHA, 'docs/knowledge/verification.md'))
    $Blobs = Get-WorkingBlobs
    $VerificationBlob = Invoke-GitValue -Arguments @('hash-object', 'docs/knowledge/verification.md')
    $VendorGitlink = Invoke-GitValue -Arguments @('rev-parse', 'HEAD:vendor/tokscale-core')
    $VendorHead = Invoke-GitValue -Arguments @('-C', 'vendor/tokscale-core', 'rev-parse', 'HEAD')
    $Prefix = Get-ProductionPrefixHash $AccountSource

    if ($Head -cne $env:CANDIDATE_SHA -or $Tree -cne $env:CANDIDATE_TREE) { throw 'Candidate identity mismatch' }
    if ($AccountObject -cne $env:CANDIDATE_ACCOUNT_BLOB -or $Blobs.Account -cne $env:CANDIDATE_ACCOUNT_BLOB) { throw 'Account source mismatch' }
    if ($StorageObject -cne $env:CANDIDATE_STORAGE_BLOB -or $Blobs.Storage -cne $env:CANDIDATE_STORAGE_BLOB) { throw 'Storage source mismatch' }
    if ($VerificationObject -cne $env:CANDIDATE_VERIFICATION_BLOB -or $VerificationBlob -cne $env:CANDIDATE_VERIFICATION_BLOB) { throw 'Verification document mismatch' }
    if ($VendorGitlink -cne $env:CANDIDATE_VENDOR_SHA -or $VendorHead -cne $env:CANDIDATE_VENDOR_SHA) { throw 'Vendor pin mismatch' }
    if ($Prefix -cne $env:CANDIDATE_PRODUCTION_PREFIX_SHA256) { throw 'Production prefix mismatch' }
    Assert-RunnerOnlyStatus

    "INITIAL_HEAD=$Head"
    "INITIAL_TREE=$Tree"
    "INITIAL_ACCOUNT_BLOB=$($Blobs.Account)"
    "INITIAL_STORAGE_BLOB=$($Blobs.Storage)"
    "INITIAL_VERIFICATION_BLOB=$VerificationBlob"
    "INITIAL_VENDOR_SHA=$VendorHead"
    "INITIAL_PRODUCTION_PREFIX_SHA256=$Prefix"
    'INITIAL_STATUS_COUNT=1'
    'INITIAL_VENDOR_STATUS_COUNT=0'
}

function Assert-FinalIdentity {
    $Head = Invoke-GitValue -Arguments @('rev-parse', 'HEAD')
    $Tree = Invoke-GitValue -Arguments @('rev-parse', 'HEAD^{tree}')
    $StorageObject = Invoke-GitValue -Arguments @('rev-parse', ('{0}:{1}' -f $env:CANDIDATE_SHA, 'crates/tb_core_ffi/src/agent_storage_windows.rs'))
    $Blobs = Get-WorkingBlobs
    $VerificationBlob = Invoke-GitValue -Arguments @('hash-object', 'docs/knowledge/verification.md')
    $VendorHead = Invoke-GitValue -Arguments @('-C', 'vendor/tokscale-core', 'rev-parse', 'HEAD')
    $Prefix = Get-ProductionPrefixHash $AccountSource
    $Status = @(Get-SuperprojectStatus)
    $VendorStatus = @(Get-VendorStatus)

    "FINAL_HEAD=$Head"
    "FINAL_TREE=$Tree"
    "FINAL_ACCOUNT_BLOB=$($Blobs.Account)"
    "FINAL_STORAGE_BLOB=$($Blobs.Storage)"
    "FINAL_VERIFICATION_BLOB=$VerificationBlob"
    "FINAL_VENDOR_SHA=$VendorHead"
    "FINAL_PRODUCTION_PREFIX_SHA256=$Prefix"
    "FINAL_STATUS_COUNT=$($Status.Count)"
    "FINAL_VENDOR_STATUS_COUNT=$($VendorStatus.Count)"

    if ($Head -cne $env:CANDIDATE_SHA -or $Tree -cne $env:CANDIDATE_TREE) { throw 'Final candidate identity mismatch' }
    if ($Blobs.Account -cne $env:CANDIDATE_ACCOUNT_BLOB) { throw 'Final account source mismatch' }
    if ($StorageObject -cne $env:CANDIDATE_STORAGE_BLOB -or $Blobs.Storage -cne $env:CANDIDATE_STORAGE_BLOB) { throw 'Final storage source mismatch' }
    if ($VerificationBlob -cne $env:CANDIDATE_VERIFICATION_BLOB) { throw 'Final verification document mismatch' }
    if ($VendorHead -cne $env:CANDIDATE_VENDOR_SHA) { throw 'Final vendor pin mismatch' }
    if ($Prefix -cne $env:CANDIDATE_PRODUCTION_PREFIX_SHA256) { throw 'Final production prefix mismatch' }
    if ($Status.Count -ne 0 -or $VendorStatus.Count -ne 0) { throw 'Final checkout is dirty' }
}

$Failed = $false
$FailureLabel = ''
$CurrentLabel = 'startup'
try {
    Assert-InitialIdentity

    foreach ($Case in $Cases) {
        $CurrentLabel = "$($Case.Label)-preflight"
        $State = Get-MutatedState $Case
        "PREFLIGHT_OK label=$($Case.Label) accountHash=$(Get-Sha256Text $State.Account) storageHash=$(Get-Sha256Text $State.Storage)"
    }

    foreach ($Case in $Cases) {
        $CurrentLabel = "$($Case.Label)-baseline"
        Assert-ExactSources
        Assert-RunnerOnlyStatus
        $BaselineBlobs = Get-WorkingBlobs
        foreach ($Test in $Case.Tests) {
            $Result = Invoke-ExactTest $Test.Name
            Assert-PassingTest $Result
            "BASELINE_RESULT label=$($Case.Label) test=$($Test.Name) exit=$($Result.ExitCode) accountHash=$($BaselineBlobs.Account) storageHash=$($BaselineBlobs.Storage)"
        }

        $State = Get-MutatedState $Case
        try {
            $CurrentLabel = "$($Case.Label)-mutant"
            Write-MutatedState $State
            Assert-MutationStatus $State.AccountChanged $State.StorageChanged
            $MutantBlobs = Get-WorkingBlobs
            if (-not $State.AccountChanged -and $MutantBlobs.Account -cne $env:CANDIDATE_ACCOUNT_BLOB) { throw 'Unexpected account mutation' }
            if (-not $State.StorageChanged -and $MutantBlobs.Storage -cne $env:CANDIDATE_STORAGE_BLOB) { throw 'Unexpected storage mutation' }
            if ($State.AccountChanged -and $MutantBlobs.Account -ceq $env:CANDIDATE_ACCOUNT_BLOB) { throw 'Account mutant hash did not change' }
            if ($State.StorageChanged -and $MutantBlobs.Storage -ceq $env:CANDIDATE_STORAGE_BLOB) { throw 'Storage mutant hash did not change' }

            foreach ($Test in $Case.Tests) {
                $Result = Invoke-ExactTest $Test.Name
                Assert-KilledTest $Result $Test.Owner
                "MUTANT_RESULT label=$($Case.Label) test=$($Test.Name) exit=$($Result.ExitCode) owner=$($Test.Owner) accountHash=$($MutantBlobs.Account) storageHash=$($MutantBlobs.Storage)"
            }
        } finally {
            Restore-ExactSources
        }

        $CurrentLabel = "$($Case.Label)-restore"
        Assert-ExactSources
        Assert-RunnerOnlyStatus
        $RestoreBlobs = Get-WorkingBlobs
        foreach ($Test in $Case.Tests) {
            $Result = Invoke-ExactTest $Test.Name
            Assert-PassingTest $Result
            "RESTORE_RESULT label=$($Case.Label) test=$($Test.Name) exit=$($Result.ExitCode) accountHash=$($RestoreBlobs.Account) storageHash=$($RestoreBlobs.Storage)"
        }
        "MUTATION_KILLED label=$($Case.Label)"
    }
} catch {
    $Failed = $true
    $FailureLabel = $CurrentLabel
} finally {
    try {
        Restore-ExactSources
    } catch {
        $Failed = $true
        if ([string]::IsNullOrEmpty($FailureLabel)) { $FailureLabel = 'final-restore' }
    }
    try {
        if ([string]::IsNullOrWhiteSpace($PSCommandPath)) { throw 'Runner path is unavailable' }
        Remove-Item -LiteralPath $PSCommandPath -Force
    } catch {
        $Failed = $true
        if ([string]::IsNullOrEmpty($FailureLabel)) { $FailureLabel = 'runner-delete' }
    }
    try {
        Assert-FinalIdentity
    } catch {
        $Failed = $true
        if ([string]::IsNullOrEmpty($FailureLabel)) { $FailureLabel = 'final-identity' }
    }
}

if ($Failed) {
    "HARNESS_FAIL label=$FailureLabel"
    exit 1
}
"HARNESS_PASS cases=$($Cases.Count)"
exit 0
