#!/bin/bash
# Print the applet names declared in the APPLETS table of main.rs.
set -eu
sed -n '/^pub const APPLETS/,/^];/p' "$1" | sed -n 's/^    ("\([a-z[]*\)".*/\1/p'
