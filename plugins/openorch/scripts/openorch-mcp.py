#!/usr/bin/env python3
"""Verify the installed runtime, then preserve stdio for the finite MCP server."""
import os
import sys
sys.dont_write_bytecode = True
from openorch import runtime, SetupError
try:
    binary = runtime().with_name("orch-mcp")
    os.execv(str(binary), [str(binary), *sys.argv[1:]])
except (SetupError, OSError, ValueError, TypeError, KeyError) as error:
    print("OpenOrch MCP: " + str(error), file=sys.stderr)
    raise SystemExit(2)
