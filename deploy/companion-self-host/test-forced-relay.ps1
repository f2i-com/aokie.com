[CmdletBinding()]
param(
    [string]$Image = 'coturn/coturn:4.14.0-r0@sha256:0c0e8fc0c263b85a134e9e4242b5e46e1f4c077c5029633511191c05b5c2c814',
    [string]$HostAddress = '',
    [int]$TurnPort = 34780,
    [int]$RelayMinPort = 55000,
    [int]$RelayMaxPort = 55010
)

$ErrorActionPreference = 'Stop'
$containerName = "aokie-coturn-forced-relay-$PID"
$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$HostAddress = $HostAddress.Trim()
if (-not $HostAddress) {
    $HostAddress = Get-NetIPConfiguration |
        Where-Object { $_.IPv4DefaultGateway -ne $null -and $_.NetAdapter.Status -eq 'Up' } |
        ForEach-Object { $_.IPv4Address.IPAddress } |
        Select-Object -First 1
}
if (-not $HostAddress) {
    throw 'No routable host IPv4 address was found for the Docker TURN relay'
}
$username = 'aokie-test'
$credential = 'aokie-test-password'

$previousTurnUrl = $env:AOKIE_TEST_TURN_URL
$previousTurnUsername = $env:AOKIE_TEST_TURN_USERNAME
$previousTurnCredential = $env:AOKIE_TEST_TURN_CREDENTIAL
$testExitCode = 1

try {
    $arguments = @(
        'run', '--detach', '--name', $containerName,
        '--read-only', '--tmpfs', '/tmp:size=16m,mode=1777',
        '--security-opt', 'no-new-privileges:true',
        '--cap-drop', 'ALL', '--cap-add', 'NET_BIND_SERVICE',
        '--publish', "${HostAddress}:${TurnPort}:3478/tcp",
        '--publish', "${HostAddress}:${TurnPort}:3478/udp",
        '--publish', "${HostAddress}:${RelayMinPort}-${RelayMaxPort}:${RelayMinPort}-${RelayMaxPort}/tcp",
        '--publish', "${HostAddress}:${RelayMinPort}-${RelayMaxPort}:${RelayMinPort}-${RelayMaxPort}/udp",
        '--entrypoint', 'turnserver', $Image,
        '-n', '--fingerprint', '--lt-cred-mech', '--realm=aokie.test',
        '--user=aokie-test:aokie-test-password', '--listening-port=3478',
        "--external-ip=$HostAddress", "--min-port=$RelayMinPort", "--max-port=$RelayMaxPort",
        '--allow-loopback-peers', '--no-multicast-peers', '--no-tls', '--no-dtls',
        '--pidfile=/tmp/turnserver.pid', '--log-file=stdout', '--simple-log'
    )
    & docker @arguments | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "coturn container failed to start (docker exit $LASTEXITCODE)"
    }

    $healthy = $false
    for ($attempt = 0; $attempt -lt 40; $attempt++) {
        $savedErrorActionPreference = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        $healthOutput = & docker exec $containerName turnutils_stunclient -p 3478 -t 1000 127.0.0.1 2>&1 | Out-String
        $healthExitCode = $LASTEXITCODE
        $ErrorActionPreference = $savedErrorActionPreference
        if ($healthExitCode -eq 0) {
            $healthy = $true
            break
        }
        Start-Sleep -Milliseconds 250
    }
    if (-not $healthy) {
        & docker logs $containerName
        throw 'coturn did not become healthy'
    }

    $env:AOKIE_TEST_TURN_URL = "turn:${HostAddress}:${TurnPort}?transport=tcp"
    $env:AOKIE_TEST_TURN_USERNAME = $username
    $env:AOKIE_TEST_TURN_CREDENTIAL = $credential
    Push-Location $repositoryRoot
    try {
        & cargo test -p aokie-media --test native_loopback native_monitor_peer_connects_over_forced_turn_relay -- --ignored --nocapture
        $testExitCode = $LASTEXITCODE
        if ($testExitCode -ne 0) {
            & docker logs $containerName
        }
    }
    finally {
        Pop-Location
    }
}
finally {
    $env:AOKIE_TEST_TURN_URL = $previousTurnUrl
    $env:AOKIE_TEST_TURN_USERNAME = $previousTurnUsername
    $env:AOKIE_TEST_TURN_CREDENTIAL = $previousTurnCredential
    $savedErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = 'SilentlyContinue'
    & docker rm --force $containerName *> $null
    $ErrorActionPreference = $savedErrorActionPreference
}

exit $testExitCode
