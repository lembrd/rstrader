#!/usr/bin/env bash
# DEPRECATED: use scripts/questdb_ctl.sh {start|stop|status|init}
echo "This script is deprecated. Use: ./scripts/questdb_ctl.sh start" >&2
"$(dirname "$0")/questdb_ctl.sh" start


