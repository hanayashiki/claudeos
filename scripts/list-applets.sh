#!/bin/bash
# Print the applet names declared in the APPLETS table of main.rs.
#
#   list-applets.sh main.rs [FEATURE...]
#
# An entry with `#[cfg(feature = "NAME")]` on the line above it is compiled into
# cbox only with that cargo feature, so it is printed only when NAME is among
# the features given, and an image gets links for exactly the applets its own
# cbox has. Any other attribute in the table stops the script rather than being
# guessed at.
set -eu
FILE="$1"
shift
sed -n '/^pub const APPLETS/,/^];/p' "$FILE" | awk -v features=" $* " '
  /^    #\[cfg\(feature = "[a-z0-9_-]+"\)\]$/ {
    split($0, quoted, "\"")
    wanted = quoted[2]
    next
  }
  /^    #/ {
    print "list-applets.sh: cannot read this attribute: " $0 > "/dev/stderr"
    exit 1
  }
  /^    \("[a-z[]*"/ {
    split($0, quoted, "\"")
    if (wanted == "" || index(features, " " wanted " ") > 0) print quoted[2]
    wanted = ""
  }
'
