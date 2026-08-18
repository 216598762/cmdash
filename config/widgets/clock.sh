#!/bin/sh
# Clock widget (interval mode). Re-run with:
#   [[workspace.widgets]]
#   type = "widget"
#   command = "/bin/sh config/widgets/clock.sh"
#   [workspace.widgets.settings]
#   mode = "interval"
#   interval_ms = "1000"
date "+%H:%M:%S"
