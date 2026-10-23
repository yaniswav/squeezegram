<#
.SYNOPSIS
    Sets the GitHub branch rules, tag rules and release environments that guard releases.

.DESCRIPTION
    Owner-only, run once (and again to check). It reads the current settings of the repository
    with the authenticated GitHub CLI and brings them to the state README.md ("Release setup")
    and .github\workflows\release.yml expect:
      - Branch ruleset on main, enforcement active: no deletion, no force push. No required
        pull request or review: the owner pushes directly.
      - Tag ruleset for refs/tags/v*: creation, update and deletion restricted; only the
        repository admin role may bypass it. channel-v* and ffmpeg-* tags stay outside it. Other
        patterns already in the ruleset (tdjson-*) are kept.
      - Environments "release" (build job) and "release-signing" (sign job): the owner is the
        required reviewer (self review allowed: a solo account has no one else), deployments only
        from tags matching v*. An environment that does not exist is created. Secrets are never
        read or changed here; only their names are listed.
    It is idempotent: once the settings are right, a run changes nothing.

    Default mode is a dry run: it prints what would change and writes nothing. -Apply performs
    the changes, then checks again and fails if anything still differs. -Verify only checks and
    exits 1 if anything differs. Only the REST endpoints rulesets, environments and
    deployment-branch-policies are written, and only by -Apply.

    What a script cannot do stays manual and is printed at the end: moving the updater signing
    secrets from "release" to "release-signing" (GitHub never returns a secret value).

.PARAMETER Repository
    GitHub repository as owner/name. The owner is the required reviewer.

.PARAMETER Apply
    Performs the planned changes.

.PARAMETER Verify
    Checks only; exits 1 if a setting differs from the desired state.

.EXAMPLE
    pwsh -File scripts\apply-github-rules.ps1
    pwsh -File scripts\apply-github-rules.ps1 -Apply
    pwsh -File scripts\apply-github-rules.ps1 -Verify
#>
[CmdletBinding()]
param(
    [string] $Repository = 'yaniswav/squeezegram',
    [switch] $Apply,
    [switch] $Verify
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$AdminRoleId = 5                      # repository role "admin" in a ruleset's bypass list
$ProtectedTagSamples = @('channel-v1', 'ffmpeg-1')   # must never be covered by the tag ruleset
$SigningSecrets = @('TAURI_SIGNING_PRIVATE_KEY', 'TAURI_SIGNING_PRIVATE_KEY_PASSWORD')

# ---- gh access --------------------------------------------------------------------------------

# Runs gh and returns its standard output. A non-zero exit code is an error that carries gh's
# own message (stderr), so a failed call can never be taken for an empty answer.
function Invoke-Gh([string[]] $Arguments, [string] $Body) {
    if ($PSBoundParameters.ContainsKey('Body')) { $output = $Body | & gh @Arguments 2>&1 }
    else { $output = & gh @Arguments 2>&1 }
    $code = $LASTEXITCODE
    $messages = @($output | Where-Object { $_ -is [System.Management.Automation.ErrorRecord] } | ForEach-Object { $_.ToString() })
    if ($code -ne 0) { throw "gh $($Arguments[0]) $($Arguments[1]) failed with exit code ${code}: $($messages -join ' ')" }
    return @($output | Where-Object { $_ -isnot [System.Management.Automation.ErrorRecord] })
}

# GET of a REST path as an object. -AllowNotFound turns an HTTP 404 into $null.
function Get-GhJson([string] $Path, [switch] $AllowNotFound) {
    try { return ((Invoke-Gh @('api', $Path)) -join "`n") | ConvertFrom-Json }
    catch {
        if ($AllowNotFound -and $_.Exception.Message -match 'HTTP 404') { return $null }
        throw
    }
}

# Names only (--jq keeps everything else out of the output); an unknown environment has none.
function Get-SecretNames([string] $Path) {
    try { return @(Invoke-Gh @('api', $Path, '--paginate', '--jq', '.secrets[].name')) }
    catch {
        if ($_.Exception.Message -match 'HTTP 404') { return @() }
        throw
    }
}

# ---- pure plan logic (tested by scripts\tests\apply-github-rules.tests.ps1) -------------------

function Get-Prop($Object, [string] $Name) {
    if ($null -eq $Object) { return $null }
    if ($Object -is [System.Collections.IDictionary]) { if ($Object.Contains($Name)) { return $Object[$Name] } else { return $null } }
    $p = $Object.PSObject.Properties[$Name]
    if ($p) { return $p.Value } else { return $null }
}

# A list without nulls, always an array.
function Get-List($Value) { return @(@($Value) | Where-Object { $null -ne $_ }) }

# The comparable part of a ruleset (as returned by the API, or as built below).
function Get-RulesetState($Ruleset) {
    $ref = Get-Prop (Get-Prop $Ruleset 'conditions') 'ref_name'
    [pscustomobject]@{
        Enforcement = [string] (Get-Prop $Ruleset 'enforcement')
        Include     = @(Get-List (Get-Prop $ref 'include') | Sort-Object)
        Exclude     = @(Get-List (Get-Prop $ref 'exclude') | Sort-Object)
        Rules       = @(Get-List (Get-Prop $Ruleset 'rules') | ForEach-Object { Get-Prop $_ 'type' } | Sort-Object)
        Bypass      = @(Get-List (Get-Prop $Ruleset 'bypass_actors') | ForEach-Object {
                "$(Get-Prop $_ 'actor_type'):$(Get-Prop $_ 'actor_id'):$(Get-Prop $_ 'bypass_mode')" } | Sort-Object)
    }
}

function New-BranchRulesetBody($Current) {
    $include = @('refs/heads/main')
    $now = @(Get-List (Get-Prop (Get-Prop (Get-Prop $Current 'conditions') 'ref_name') 'include'))
    if ($now.Count -eq 1 -and $now[0] -in '~DEFAULT_BRANCH', 'refs/heads/main') { $include = $now }
    return @{
        name          = 'Protect main'
        target        = 'branch'
        enforcement   = 'active'
        conditions    = @{ ref_name = @{ include = $include; exclude = @() } }
        bypass_actors = @()
        rules         = @(@{ type = 'deletion' }, @{ type = 'non_fast_forward' })
    }
}

# Keeps the patterns already there (tdjson-*), drops any that would cover channel-v* or ffmpeg-*
# tags, and makes sure refs/tags/v* is in.
function New-TagRulesetBody($Current) {
    $now = @(Get-List (Get-Prop (Get-Prop (Get-Prop $Current 'conditions') 'ref_name') 'include'))
    $keep = @($now | Where-Object {
            $pattern = $_
            -not ($ProtectedTagSamples | Where-Object { "refs/tags/$_" -like $pattern })
        })
    if ($keep -notcontains 'refs/tags/v*') { $keep = @('refs/tags/v*') + $keep }
    return @{
        name          = 'Protect release tags'
        target        = 'tag'
        enforcement   = 'active'
        conditions    = @{ ref_name = @{ include = $keep; exclude = @() } }
        bypass_actors = @(@{ actor_id = $AdminRoleId; actor_type = 'RepositoryRole'; bypass_mode = 'always' })
        rules         = @(@{ type = 'creation' }, @{ type = 'update' }, @{ type = 'deletion' })
    }
}

# A step is one write: Title, Details (the before -> after lines), Method, Path, Body.
function New-Step([string] $Title, [string[]] $Details, [string] $Method, [string] $Path, $Body) {
    return [pscustomobject]@{ Title = $Title; Details = @($Details); Method = $Method; Path = $Path; Body = $Body }
}

# Steps that bring Current (a ruleset object, or $null when absent) to Desired (a body).
function Get-RulesetPlan([string] $Repo, $Current, $Desired) {
    $want = Get-RulesetState $Desired
    $name = $Desired.name
    $summary = "enforcement $($want.Enforcement); include [$($want.Include -join ', ')]; rules [$($want.Rules -join ', ')]; bypass [$($want.Bypass -join ', ')]"
    if ($null -eq $Current) {
        return @(New-Step "Create $($Desired.target) ruleset '$name'" @("none -> $summary") 'POST' "repos/$Repo/rulesets" $Desired)
    }
    $have = Get-RulesetState $Current
    $details = @()
    foreach ($field in 'Enforcement', 'Include', 'Exclude', 'Rules', 'Bypass') {
        $a = @($have.$field) -join ', '
        $b = @($want.$field) -join ', '
        if ($a -ne $b) { $details += "${field}: [$a] -> [$b]" }
    }
    if ($details.Count -eq 0) { return @() }
    return @(New-Step "Update $($Desired.target) ruleset '$name'" $details 'PUT' "repos/$Repo/rulesets/$(Get-Prop $Current 'id')" $Desired)
}

function Get-EnvRule($Environment, [string] $Type) {
    return @(Get-List (Get-Prop $Environment 'protection_rules') | Where-Object { (Get-Prop $_ 'type') -eq $Type }) | Select-Object -First 1
}

# Steps for one environment: the environment itself (reviewer, deployment rule), then its
# deployment policies, which must be the single tag pattern v*. Environment is $null when absent.
function Get-EnvironmentPlan([string] $Repo, [string] $Name, $Environment, $Policies, $Reviewer) {
    $steps = @()
    $path = "repos/$Repo/environments/$Name"
    $details = @()
    $wait = 0
    $adminsBypass = $null
    if ($null -eq $Environment) {
        $details += "none -> required reviewer $($Reviewer.Login), self review allowed, custom deployment policies"
    }
    else {
        $review = Get-EnvRule $Environment 'required_reviewers'
        $timer = Get-EnvRule $Environment 'wait_timer'
        if ($timer) { $wait = [int] (Get-Prop $timer 'wait_timer') }
        $adminsBypass = Get-Prop $Environment 'can_admins_bypass'
        $have = @(Get-List (Get-Prop $review 'reviewers') | ForEach-Object { "$(Get-Prop $_ 'type'):$(Get-Prop (Get-Prop $_ 'reviewer') 'id')" } | Sort-Object)
        $want = @("User:$($Reviewer.Id)")
        if (($have -join ', ') -ne ($want -join ', ')) { $details += "required reviewers: [$($have -join ', ')] -> [$($Reviewer.Login)]" }
        if ($review -and (Get-Prop $review 'prevent_self_review') -eq $true) { $details += 'prevent_self_review: true -> false' }
        $dbp = Get-Prop $Environment 'deployment_branch_policy'
        if ($null -eq $dbp -or (Get-Prop $dbp 'protected_branches') -ne $false -or (Get-Prop $dbp 'custom_branch_policies') -ne $true) {
            $details += 'deployment branches: not "selected branches and tags" -> "selected branches and tags"'
        }
    }
    if ($details.Count -gt 0) {
        $body = @{
            wait_timer              = $wait
            prevent_self_review     = $false
            reviewers               = @(@{ type = 'User'; id = $Reviewer.Id })
            deployment_branch_policy = @{ protected_branches = $false; custom_branch_policies = $true }
        }
        if ($null -ne $adminsBypass) { $body.can_admins_bypass = $adminsBypass }
        $verb = if ($null -eq $Environment) { 'Create' } else { 'Update' }
        $steps += New-Step "$verb environment '$Name'" $details 'PUT' $path $body
    }

    $all = @(Get-List $Policies)
    $good = @($all | Where-Object { (Get-Prop $_ 'type') -eq 'tag' -and (Get-Prop $_ 'name') -eq 'v*' })
    foreach ($p in $all | Where-Object { $good -notcontains $_ }) {
        $steps += New-Step "Delete deployment policy of '$Name'" @("$(Get-Prop $p 'type'):$(Get-Prop $p 'name') -> removed") 'DELETE' "$path/deployment-branch-policies/$(Get-Prop $p 'id')" $null
    }
    if ($good.Count -eq 0) {
        $steps += New-Step "Add deployment policy to '$Name'" @('none -> tag:v*') 'POST' "$path/deployment-branch-policies" @{ name = 'v*'; type = 'tag' }
    }
    return $steps
}

# What stays manual: the updater signing secrets belong in release-signing only.
function Get-SecretReport([string[]] $Release, [string[]] $Signing, [string[]] $Repo) {
    foreach ($name in $SigningSecrets) {
        $todo = @()
        if ($Signing -notcontains $name) { $todo += 'add to release-signing (enter the value again: GitHub never shows it)' }
        if ($Release -contains $name) { $todo += 'then delete from release' }
        if ($Repo -contains $name) { $todo += 'delete from the repository secrets' }
        [pscustomobject]@{
            Name = $name
            Where = "release=$(if ($Release -contains $name) { 'yes' } else { 'no' }) release-signing=$(if ($Signing -contains $name) { 'yes' } else { 'no' }) repository=$(if ($Repo -contains $name) { 'yes' } else { 'no' })"
            Todo = $todo
        }
    }
}

# ---- run ---------------------------------------------------------------------------------------

# Dot-sourcing (the tests) loads the functions only.
if ($MyInvocation.InvocationName -eq '.') { return }

function Get-FullPlan {
    $ruleset = @{}
    foreach ($item in (Get-GhJson "repos/$Repository/rulesets?per_page=100")) {
        $ruleset[$item.name] = Get-GhJson "repos/$Repository/rulesets/$($item.id)"
    }
    $branch = New-BranchRulesetBody $ruleset['Protect main']
    $tag = New-TagRulesetBody $ruleset['Protect release tags']
    $plan = @()
    $plan += Get-RulesetPlan $Repository $ruleset['Protect main'] $branch
    $plan += Get-RulesetPlan $Repository $ruleset['Protect release tags'] $tag
    foreach ($name in 'release', 'release-signing') {
        $environment = Get-GhJson "repos/$Repository/environments/$name" -AllowNotFound
        $policies = @()
        if ($environment) { $policies = @((Get-GhJson "repos/$Repository/environments/$name/deployment-branch-policies?per_page=100").branch_policies) }
        $plan += Get-EnvironmentPlan $Repository $name $environment $policies $script:Reviewer
    }
    return $plan
}

if ($Apply -and $Verify) { throw '-Apply and -Verify cannot be combined' }
if ($Repository -notmatch '^([\w.-]+)/[\w.-]+$') { throw "Repository '$Repository' is not owner/name" }
$owner = $Matches[1]

$repo = Get-GhJson "repos/$Repository"
if (-not $repo.permissions.admin) { throw "The authenticated gh account is not an admin of $Repository" }
$user = Get-GhJson "users/$owner"
$script:Reviewer = [pscustomobject]@{ Login = $user.login; Id = [int64] $user.id }

$mode = if ($Apply) { 'APPLY' } elseif ($Verify) { 'VERIFY' } else { 'DRY RUN' }
Write-Host "GitHub rules for $Repository ($mode)"
$plan = @(Get-FullPlan)

if ($plan.Count -eq 0) { Write-Host 'Branch ruleset, tag ruleset and both environments already match: nothing to change.' }
foreach ($step in $plan) {
    Write-Host "* $($step.Title)  [$($step.Method) $($step.Path)]"
    foreach ($line in $step.Details) { Write-Host "    $line" }
}

if ($Apply -and $plan.Count -gt 0) {
    foreach ($step in $plan) {
        Write-Host "Applying: $($step.Title)"
        $arguments = @('api', '-X', $step.Method, $step.Path)
        if ($null -ne $step.Body) {
            $null = Invoke-Gh ($arguments + @('--input', '-')) -Body (ConvertTo-Json -InputObject $step.Body -Depth 10 -Compress)
        }
        else { $null = Invoke-Gh $arguments }
    }
    $left = @(Get-FullPlan)
    if ($left.Count -gt 0) { throw "After applying, $($left.Count) step(s) still differ: $(($left | ForEach-Object Title) -join '; ')" }
    Write-Host 'Applied and re-checked: everything matches.'
}

# Names only, never values.
$release = @(Get-SecretNames "repos/$Repository/environments/release/secrets")
$signing = @(Get-SecretNames "repos/$Repository/environments/release-signing/secrets")
$repoSecrets = @(Get-SecretNames "repos/$Repository/actions/secrets")
Write-Host ''
Write-Host 'Manual (the updater signing key belongs in release-signing only; add it there before deleting it elsewhere):'
Write-Host "  secret names in release: $($release -join ', ')"
Write-Host "  secret names in release-signing: $(if ($signing.Count) { $signing -join ', ' } else { '(none or environment missing)' })"
Write-Host "  repository secret names: $(if ($repoSecrets.Count) { $repoSecrets -join ', ' } else { '(none)' })"
$open = $false
foreach ($s in Get-SecretReport $release $signing $repoSecrets) {
    if ($s.Todo.Count -gt 0) { $open = $true; Write-Host "  - $($s.Name) ($($s.Where)): $($s.Todo -join ', ')" }
    else { Write-Host "  - $($s.Name) ($($s.Where)): done" }
}

if ($Verify -and ($plan.Count -gt 0 -or $open)) {
    Write-Host ''
    Write-Host "Verify failed: $($plan.Count) setting(s) differ$(if ($open) { ', signing secrets not yet moved' })."
    exit 1
}
if ($Verify) { Write-Host 'Verify passed.' }
