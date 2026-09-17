#!/usr/bin/env bash
set -e

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$DIR"

# Auto-backup non-symlink compose files to .direct if not already created
if [ ! -f "docker-compose.yml.direct" ] && [ -f "docker-compose.yml" ] && [ ! -L "docker-compose.yml" ]; then
    echo "Creating initial docker-compose.yml.direct backup..."
    cp docker-compose.yml docker-compose.yml.direct
fi

if [ ! -f "docker-compose.override.yml.direct" ] && [ -f "docker-compose.override.yml" ] && [ ! -L "docker-compose.override.yml" ]; then
    echo "Creating initial docker-compose.override.yml.direct backup..."
    cp docker-compose.override.yml docker-compose.override.yml.direct
fi

get_active_provider() {
    if [ -L "docker-compose.yml" ]; then
        TARGET=$(readlink "docker-compose.yml")
        case "$TARGET" in
            *protonvpn*) echo "ProtonVPN" ;;
            *nordvpn*)   echo "NordVPN" ;;
            *direct*)    echo "Direct (No VPN)" ;;
            *)           echo "Custom/Unknown ($TARGET)" ;;
        esac
    else
        echo "None (docker-compose.yml is not a symlink)"
    fi
}

check_status() {
    ACTIVE=$(get_active_provider)
    echo "=========================================="
    echo "  VPN / Network Status & Egress Inspection"
    echo "=========================================="
    echo "Active Target Config : $ACTIVE"
    echo ""

    if [ "$ACTIVE" = "Direct (No VPN)" ] || [ "$ACTIVE" = "None (docker-compose.yml is not a symlink)" ]; then
        if docker ps --format '{{.Names}}' | grep -q "^webspeak3$"; then
            echo "webspeak3 Container : Running (Direct Host Network)"
            echo "Public Egress Info:"
            echo "------------------------------------------"
            docker exec webspeak3 node -e "fetch('https://ipinfo.io/json').then(r=>r.text()).then(console.log)" 2>/dev/null || \
            curl -s https://ipinfo.io 2>/dev/null || \
            echo "Failed to query IP."
        else
            echo "webspeak3 Container : Not Running"
        fi
    else
        if docker ps --format '{{.Names}}' | grep -q "^vpn_gateway$"; then
            echo "VPN Gateway Container: Running"
            echo "Public Egress Info:"
            echo "------------------------------------------"
            docker exec vpn_gateway wget -qO- https://ipinfo.io 2>/dev/null || \
            docker exec vpn_gateway curl -s https://ipinfo.io 2>/dev/null || \
            echo "Failed to query IP from inside vpn_gateway."
        else
            echo "VPN Gateway Container: Not Running"
        fi
    fi
    echo "=========================================="
}

wait_for_healthy() {
    ACTIVE=$(get_active_provider)
    echo -n "Waiting for network connectivity..."

    CONTAINER="vpn_gateway"
    if [ "$ACTIVE" = "Direct (No VPN)" ] || [ "$ACTIVE" = "None (docker-compose.yml is not a symlink)" ]; then
        CONTAINER="webspeak3"
    fi

    for i in {1..15}; do
        if [ "$CONTAINER" = "webspeak3" ]; then
            if docker exec webspeak3 node -e "fetch('https://ipinfo.io').then(r=>{if(!r.ok)process.exit(1)}).catch(()=>process.exit(1))" >/dev/null 2>&1; then
                echo " Connected!"
                return 0
            fi
        else
            if docker exec vpn_gateway wget -qO- https://ipinfo.io >/dev/null 2>&1 || \
               docker exec vpn_gateway curl -s https://ipinfo.io >/dev/null 2>&1; then
                echo " Connected!"
                return 0
            fi
        fi
        echo -n "."
        sleep 1
    done
    echo " Timeout waiting for IP response."
    return 1
}

switch_provider() {
    PROVIDER="$1"
    case "$PROVIDER" in
        protonvpn|proton)
            echo "Switching symlinks to ProtonVPN..."
            ln -sf docker-compose.yml.protonvpn docker-compose.yml
            ln -sf docker-compose.override.yml.protonvpn docker-compose.override.yml
            ;;
        nordvpn|nord)
            echo "Switching symlinks to NordVPN..."
            ln -sf docker-compose.yml.nordvpn docker-compose.yml
            ln -sf docker-compose.override.yml.nordvpn docker-compose.override.yml
            ;;
        direct)
            if [ ! -f "docker-compose.yml.direct" ] || [ ! -f "docker-compose.override.yml.direct" ]; then
                echo "Error: docker-compose.yml.direct or docker-compose.override.yml.direct missing."
                exit 1
            fi
            echo "Switching symlinks to Direct (No VPN)..."
            ln -sf docker-compose.yml.direct docker-compose.yml
            ln -sf docker-compose.override.yml.direct docker-compose.override.yml
            ;;
        *)
            echo "Error: Unknown provider '$PROVIDER'. Use 'protonvpn', 'nordvpn', or 'direct'."
            exit 1
            ;;
    esac

    echo "Recreating container stack..."
    docker compose down --remove-orphans || true
    docker compose up -d --force-recreate
    wait_for_healthy || true
    echo ""
    check_status
}

show_logs() {
    ACTIVE=$(get_active_provider)
    if [ "$ACTIVE" = "ProtonVPN" ] || [ "$ACTIVE" = "NordVPN" ]; then
        echo "Tailing vpn_gateway logs (Ctrl+C to exit)..."
        docker compose logs -f vpn_gateway
    else
        echo "Tailing webspeak3 logs (Ctrl+C to exit)..."
        docker compose logs -f webspeak3
    fi
}

restart_stack() {
    echo "Restarting container stack..."
    docker compose restart
    wait_for_healthy || true
    echo ""
    check_status
}

stop_stack() {
    echo "Stopping container stack..."
    docker compose down --remove-orphans
}

show_help() {
    echo "Usage: $0 {status|protonvpn|nordvpn|direct|logs|restart|stop}"
    echo ""
    echo "Commands:"
    echo "  status                Show active provider and public egress IP"
    echo "  protonvpn (proton)    Switch target to ProtonVPN and recreate stack"
    echo "  nordvpn (nord)        Switch target to NordVPN and recreate stack"
    echo "  direct                Switch target to Direct (No VPN) and recreate stack"
    echo "  logs                  Tail logs for active container/gateway"
    echo "  restart               Restart containers without changing provider"
    echo "  stop                  Bring down the docker compose stack"
}

case "$1" in
    status)
        check_status
        ;;
    protonvpn|proton)
        switch_provider "protonvpn"
        ;;
    nordvpn|nord)
        switch_provider "nordvpn"
        ;;
    direct)
        switch_provider "direct"
        ;;
    logs)
        show_logs
        ;;
    restart)
        restart_stack
        ;;
    stop)
        stop_stack
        ;;
    *)
        show_help
        exit 1
        ;;
esac
