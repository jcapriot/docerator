# some common utilities common to testing.
import sys
import textwrap

py313 = sys.version_info >= (3, 13)

def py313_docstrip(text):
    if py313:
        text=text.split("\n", maxsplit=1)
        return "\n".join([text[0].strip(), textwrap.dedent(text[1])])
    return text