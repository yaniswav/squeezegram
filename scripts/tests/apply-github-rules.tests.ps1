<#
.SYNOPSIS
    Pester checks for the plan logic of apply-github-rules.ps1 (desired state against current JSON).
    Written for Pester 3.4 and 5: BeforeEach loads the script (functions only) and asserts throw.
.EXAMPLE
    Invoke-Pester -Path ./scripts/tests/apply-github-rules.tests.ps1 -EnableExit
#>

Describe 'apply-github-rules plan' {
    BeforeEach {
        . (Join-Path (Split-Path -Parent $PSScriptRoot) 'apply-github-rules.ps1')
        function Assert-Equal($Actual, $Expected, [string] $What) {
            if ("$Actual" -ne "$Expected") { throw "$What : expected '$Expected', got '$Actual'" }
        }
        $script:Repo = 'o/r'
        $script:Me = [pscustomobject]@{ Login = 'o'; Id = 7 }
        $script:BranchNow = '{"id":1,"enforcement":"active","conditions":{"ref_name":{"include":["~DEFAULT_BRANCH"],"exclude":[]}},"bypass_actors":[],"rules":[{"type":"deletion"},{"type":"non_fast_forward"}]}' | ConvertFrom-Json
        $script:TagNow = '{"id":2,"enforcement":"active","conditions":{"ref_name":{"include":["refs/tags/v*","refs/tags/tdjson-*"],"exclude":[]}},"bypass_actors":[{"actor_id":5,"actor_type":"RepositoryRole","bypass_mode":"always"}],"rules":[{"type":"creation"},{"type":"update"},{"type":"deletion"}]}' | ConvertFrom-Json
        $script:EnvNow = '{"can_admins_bypass":true,"protection_rules":[{"type":"required_reviewers","prevent_self_review":false,"reviewers":[{"type":"User","reviewer":{"id":7}}]},{"type":"branch_policy"}],"deployment_branch_policy":{"protected_branches":false,"custom_branch_policies":true}}' | ConvertFrom-Json
        $script:PolicyNow = '[{"id":9,"name":"v*","type":"tag"}]' | ConvertFrom-Json
    }

    It 'plans nothing for a ruleset that is already right' {
        Assert-Equal (@(Get-RulesetPlan $Repo $BranchNow (New-BranchRulesetBody $BranchNow))).Count 0 'branch'
        Assert-Equal (@(Get-RulesetPlan $Repo $TagNow (New-TagRulesetBody $TagNow))).Count 0 'tag'
    }

    It 'creates an absent ruleset and updates a wrong one' {
        $step = @(Get-RulesetPlan $Repo $null (New-BranchRulesetBody $null))
        Assert-Equal $step.Count 1 'create count'
        Assert-Equal $step[0].Method 'POST' 'create method'
        Assert-Equal $step[0].Path 'repos/o/r/rulesets' 'create path'
        Assert-Equal ($step[0].Body.conditions.ref_name.include -join ',') 'refs/heads/main' 'new branch include'

        $bad = '{"id":1,"enforcement":"evaluate","conditions":{"ref_name":{"include":["~DEFAULT_BRANCH"],"exclude":[]}},"bypass_actors":[],"rules":[{"type":"deletion"},{"type":"pull_request"}]}' | ConvertFrom-Json
        $step = @(Get-RulesetPlan $Repo $bad (New-BranchRulesetBody $bad))
        Assert-Equal $step.Count 1 'update count'
        Assert-Equal $step[0].Method 'PUT' 'update method'
        Assert-Equal $step[0].Path 'repos/o/r/rulesets/1' 'update path'
        Assert-Equal (($step[0].Body.rules | ForEach-Object { $_.type }) -join ',') 'deletion,non_fast_forward' 'rules have no pull_request'
        if (($step[0].Details -join "`n") -notmatch 'Enforcement') { throw 'enforcement change not listed' }
    }

    It 'keeps channel and ffmpeg tags outside the tag ruleset and bypass admin only' {
        $wide = '{"id":2,"enforcement":"active","conditions":{"ref_name":{"include":["refs/tags/*","refs/tags/channel-v*"],"exclude":[]}},"bypass_actors":[{"actor_id":1,"actor_type":"OrganizationAdmin","bypass_mode":"always"}],"rules":[{"type":"creation"}]}' | ConvertFrom-Json
        $body = New-TagRulesetBody $wide
        Assert-Equal ($body.conditions.ref_name.include -join ',') 'refs/tags/v*' 'include after dropping wide patterns'
        Assert-Equal $body.bypass_actors.Count 1 'one bypass actor'
        Assert-Equal $body.bypass_actors[0].actor_id 5 'admin role only'
        Assert-Equal @(Get-RulesetPlan $Repo $wide $body).Count 1 'wide ruleset is updated'
        $empty = New-TagRulesetBody $null
        Assert-Equal ($empty.conditions.ref_name.include -join ',') 'refs/tags/v*' 'new tag ruleset include'
        Assert-Equal (($empty.rules | ForEach-Object { $_.type }) -join ',') 'creation,update,deletion' 'tag rules'
    }

    It 'plans nothing for a correct environment' {
        Assert-Equal @(Get-EnvironmentPlan $Repo 'release' $EnvNow $PolicyNow $Me).Count 0 'steps'
    }

    It 'creates a missing environment with its reviewer and v* tag policy' {
        $steps = @(Get-EnvironmentPlan $Repo 'release-signing' $null @() $Me)
        Assert-Equal $steps.Count 2 'steps'
        Assert-Equal $steps[0].Method 'PUT' 'env method'
        Assert-Equal $steps[0].Path 'repos/o/r/environments/release-signing' 'env path'
        Assert-Equal $steps[0].Body.reviewers[0].id 7 'reviewer id'
        Assert-Equal $steps[0].Body.prevent_self_review $false 'self review allowed'
        Assert-Equal $steps[1].Method 'POST' 'policy method'
        Assert-Equal $steps[1].Body.type 'tag' 'policy type'
        Assert-Equal $steps[1].Body.name 'v*' 'policy name'
    }

    It 'fixes reviewers, self review, branch rule and wrong policies' {
        $env = '{"protection_rules":[{"type":"required_reviewers","prevent_self_review":true,"reviewers":[{"type":"User","reviewer":{"id":99}}]}],"deployment_branch_policy":{"protected_branches":true,"custom_branch_policies":false}}' | ConvertFrom-Json
        $policies = '[{"id":3,"name":"main","type":"branch"},{"id":4,"name":"v*","type":"branch"}]' | ConvertFrom-Json
        $steps = @(Get-EnvironmentPlan $Repo 'release' $env $policies $Me)
        Assert-Equal $steps.Count 4 'steps'
        Assert-Equal $steps[0].Method 'PUT' 'env updated'
        Assert-Equal $steps[0].Details.Count 3 'reviewer, self review, branch rule'
        Assert-Equal (@($steps | Where-Object { $_.Method -eq 'DELETE' }).Count) 2 'both wrong policies deleted'
        Assert-Equal $steps[1].Path 'repos/o/r/environments/release/deployment-branch-policies/3' 'delete path'
        Assert-Equal $steps[3].Method 'POST' 'tag policy added'
    }

    It 'lists which signing secret names still need moving' {
        $report = @(Get-SecretReport @('A', 'TAURI_SIGNING_PRIVATE_KEY', 'TAURI_SIGNING_PRIVATE_KEY_PASSWORD') @() @())
        Assert-Equal $report.Count 2 'two secrets'
        Assert-Equal $report[0].Todo.Count 2 'add then delete'
        $done = @(Get-SecretReport @('A') @('TAURI_SIGNING_PRIVATE_KEY', 'TAURI_SIGNING_PRIVATE_KEY_PASSWORD') @())
        Assert-Equal (@($done | ForEach-Object { $_.Todo.Count } | Measure-Object -Sum).Sum) 0 'nothing left'
    }
}
