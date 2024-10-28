
import re

REPLACE_REGEX = re.compile(r"%\((?P<replace_key>.*)\)")
REPLACE_ARG_SPLIT_REGEX = re.compile(r"\s*,\s*")

class DocstringInheritWarning(ImportWarning):
    pass


class DoceratorParsingError(SyntaxError):
    pass

DEBUG_LEVEL = 0

def set_debug_level(level: int) -> bool:
    global DEBUG_LEVEL
    DEBUG_LEVEL = int(level)
    return DEBUG_LEVEL

def get_debug_level():
    return DEBUG_LEVEL