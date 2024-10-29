
import re

REPLACE_REGEX: re.Pattern = re.compile(r"%\((?P<replace_key>.*)\)")
REPLACE_ARG_SPLIT_REGEX: re.Pattern = re.compile(r"\s*,\s*")

class DocstringInheritWarning(ImportWarning):
    pass


class DoceratorParsingError(SyntaxError):
    pass

DEBUG_LEVEL: int = 0

def set_debug_level(level: int) -> int:
    global DEBUG_LEVEL
    DEBUG_LEVEL = int(level)
    return DEBUG_LEVEL

def get_debug_level() -> int:
    return DEBUG_LEVEL