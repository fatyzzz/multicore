param(
    [string]$Address = "127.0.0.1",
    [int]$Port = 18787,
    [ValidateSet("empty", "ready", "connected", "error")]
    [string]$InitialState = "ready",
    [int]$SelectionDelayMilliseconds = 0,
    [string]$AuthorizationValue = "preview-local-only"
)

$ErrorActionPreference = "Stop"

if ($Address -ne "127.0.0.1") {
    throw "The preview fixture binds only to 127.0.0.1."
}
if ($SelectionDelayMilliseconds -lt 0 -or $SelectionDelayMilliseconds -gt 15000) {
    throw "SelectionDelayMilliseconds must be between 0 and 15000."
}

$listener = [System.Net.Sockets.TcpListener]::new(
    [System.Net.IPAddress]::Loopback,
    $Port
)
$listener.Start()

$state = $InitialState
$selectedNode = "node-auto"
$utf8 = [System.Text.UTF8Encoding]::new($false)
$copy = @'
{
  "automatic": "\ud83c\udf0d Auto",
  "route_a": "\ud83c\udde6\ud83c\uddf1 Albania [al]",
  "route_b": "\ud83c\udde9\ud83c\uddea Germany [de]",
  "reserve_route": "\ud83c\uddf8\ud83c\uddea Sweden [se]",
  "main_group": "\ud83c\udf0d \u0421\u0435\u0440\u0432\u0435\u0440",
  "reserve_group": "\ud83c\udfae \u0418\u0433\u0440\u044b",
  "manual_route": "Без VPN",
  "local_profile": "\u041b\u043e\u043a\u0430\u043b\u044c\u043d\u044b\u0439 \u043f\u0440\u043e\u0444\u0438\u043b\u044c",
  "error": "\u041d\u0435 \u0443\u0434\u0430\u043b\u043e\u0441\u044c \u043f\u0440\u0438\u043c\u0435\u043d\u0438\u0442\u044c \u043b\u043e\u043a\u0430\u043b\u044c\u043d\u0443\u044e \u043d\u0430\u0441\u0442\u0440\u043e\u0439\u043a\u0443. \u041f\u043e\u0432\u0442\u043e\u0440\u0438\u0442\u0435 \u043f\u043e\u043f\u044b\u0442\u043a\u0443.",
  "log_xray": "\u041b\u043e\u043a\u0430\u043b\u044c\u043d\u044b\u0435 SOCKS-\u043c\u043e\u0441\u0442\u044b \u0433\u043e\u0442\u043e\u0432\u044b.",
  "log_mihomo": "\u041a\u043e\u043d\u0442\u0440\u043e\u043b\u043b\u0435\u0440 \u0438 TUN MultiCore \u0433\u043e\u0442\u043e\u0432\u044b."
}
'@ | ConvertFrom-Json

function Get-Catalog {
    if ($state -eq "empty") {
        return @{ revision = 1; groups = @() }
    }

    $nodes = @(
        @{ id = "node-auto"; label = $copy.automatic; selected = ($selectedNode -eq "node-auto"); delay_ms = 34 },
        @{ id = "node-primary"; label = $copy.route_a; selected = ($selectedNode -eq "node-primary"); delay_ms = 48 },
        @{ id = "node-backup"; label = $copy.route_b; selected = ($selectedNode -eq "node-backup"); delay_ms = 81 },
        @{ id = "node-reserve"; label = $copy.reserve_route; selected = ($selectedNode -eq "node-reserve"); delay_ms = 126 }
    )
    return @{
        revision = 7
        groups = @(
            @{ id = "group-main"; label = $copy.main_group; selected = $true; nodes = $nodes },
            @{ id = "group-reserve"; label = $copy.reserve_group; selected = $false; nodes = @(
                @{ id = "node-reserve-auto"; label = $copy.automatic; selected = $true; delay_ms = 57 },
                @{ id = "node-reserve-manual"; label = $copy.manual_route; selected = $false; delay_ms = 93 }
            ) }
        )
    }
}

function Get-Status {
    $node = switch ($selectedNode) {
        "node-primary" { $copy.route_a }
        "node-backup" { $copy.route_b }
        "node-reserve" { $copy.reserve_route }
        default { $copy.automatic }
    }
    return @{
        state = $state
        profile = if ($state -eq "empty") { $null } else { $copy.local_profile }
        current_node = if ($state -eq "empty") { $null } else { $node }
        message = if ($state -eq "error") { $copy.error } else { $null }
        degraded = $false
        subscription = if ($state -eq "empty") { $null } else {
            @{
                source_name = "subscription.example"
                downloaded_bytes = 938375741110
                total_bytes = $null
                expires_at_unix = 1792851157
                updated_at_unix = 1789405200
                refresh_available = $true
            }
        }
    }
}

function Send-JsonResponse([System.Net.Sockets.NetworkStream]$Stream, [int]$Status, $Body) {
    $json = $Body | ConvertTo-Json -Depth 8 -Compress
    $bodyBytes = $utf8.GetBytes($json)
    $reason = if ($Status -eq 200) { "OK" } elseif ($Status -eq 401) { "Unauthorized" } else { "Not Found" }
    $headers = "HTTP/1.1 $Status $reason`r`nContent-Type: application/json; charset=utf-8`r`nContent-Length: $($bodyBytes.Length)`r`nConnection: close`r`n`r`n"
    $headerBytes = [System.Text.Encoding]::ASCII.GetBytes($headers)
    $Stream.Write($headerBytes, 0, $headerBytes.Length)
    $Stream.Write($bodyBytes, 0, $bodyBytes.Length)
    $Stream.Flush()
}

Write-Host "MultiCore preview fixture listening on http://${Address}:$Port in $InitialState state"

try {
    while ($true) {
        $client = $listener.AcceptTcpClient()
        try {
            $stream = $client.GetStream()
            $reader = [System.IO.StreamReader]::new($stream, $utf8, $false, 4096, $true)
            $requestLine = $reader.ReadLine()
            if ([string]::IsNullOrWhiteSpace($requestLine)) {
                continue
            }

            $authorization = ""
            $contentLength = 0
            while ($true) {
                $line = $reader.ReadLine()
                if ([string]::IsNullOrEmpty($line)) { break }
                if ($line.StartsWith("Authorization:", [System.StringComparison]::OrdinalIgnoreCase)) {
                    $authorization = $line.Substring($line.IndexOf(":") + 1).Trim()
                }
                if ($line.StartsWith("Content-Length:", [System.StringComparison]::OrdinalIgnoreCase)) {
                    $contentLength = [int]$line.Substring($line.IndexOf(":") + 1).Trim()
                }
            }

            $body = ""
            if ($contentLength -gt 0) {
                $buffer = [char[]]::new($contentLength)
                $read = $reader.ReadBlock($buffer, 0, $contentLength)
                if ($read -gt 0) {
                    $body = -join $buffer[0..($read - 1)]
                }
            }

            if ($authorization -cne "Bearer $AuthorizationValue") {
                Send-JsonResponse $stream 401 @{ code = "unauthorized"; message = "Unauthorized"; correlation_id = "preview" }
                continue
            }

            $parts = $requestLine.Split(" ")
            $method = $parts[0]
            $path = $parts[1]

            if ($method -eq "GET" -and $path.StartsWith("/v1/status")) {
                Send-JsonResponse $stream 200 (Get-Status)
            } elseif ($method -eq "POST" -and $path.StartsWith("/v1/connect")) {
                $state = "connected"
                Send-JsonResponse $stream 200 (Get-Status)
            } elseif ($method -eq "POST" -and $path.StartsWith("/v1/disconnect")) {
                $state = "ready"
                Send-JsonResponse $stream 200 (Get-Status)
            } elseif ($method -eq "POST" -and $path.StartsWith("/v1/subscriptions/import")) {
                $state = "ready"
                Send-JsonResponse $stream 200 (Get-Status)
            } elseif ($method -eq "POST" -and $path.StartsWith("/v1/subscriptions/refresh")) {
                Send-JsonResponse $stream 200 (Get-Status)
            } elseif ($method -eq "GET" -and $path.StartsWith("/v1/catalog")) {
                Send-JsonResponse $stream 200 (Get-Catalog)
            } elseif ($method -eq "GET" -and $path.StartsWith("/v1/events")) {
                Send-JsonResponse $stream 200 @{
                    epoch = "preview-session"
                    events = @()
                }
            } elseif ($method -eq "GET" -and $path.StartsWith("/v1/diagnostics")) {
                Send-JsonResponse $stream 200 @{
                    xray = "ready"
                    mihomo = "ready"
                    tun = "ready"
                    mappings = @(
                        @{ proxy_name = "Sweden [se]"; address = "127.0.0.1:32001"; xray_label = "Xray · Sweden" },
                        @{ proxy_name = "Germany [de]"; address = "127.0.0.1:32002"; xray_label = "Xray · Germany" }
                    )
                    logs = @(
                        @{ id = 1; timestamp = "21:48:11"; component = "xray"; level = "info"; message = $copy.log_xray },
                        @{ id = 2; timestamp = "21:48:12"; component = "mihomo"; level = "info"; message = $copy.log_mihomo }
                    )
                }
            } elseif ($method -eq "PUT" -and $path.StartsWith("/v1/selections/")) {
                if ($body) {
                    $selection = $body | ConvertFrom-Json
                    $selectedNode = [string]$selection.node_id
                }
                if ($SelectionDelayMilliseconds -gt 0) {
                    Start-Sleep -Milliseconds $SelectionDelayMilliseconds
                }
                Send-JsonResponse $stream 200 (Get-Catalog)
            } else {
                Send-JsonResponse $stream 404 @{ code = "not_found"; message = "Not found"; correlation_id = "preview" }
            }
        } finally {
            $client.Dispose()
        }
    }
} finally {
    $listener.Stop()
}
