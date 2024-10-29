import inspect
import itertools
import re
import textwrap
from typing import Optional, Union

from docerator import get_debug_level, DoceratorParsingError
from docerator._base import REPLACE_REGEX
from docerator._params import DescribedParameter
from docerator.parsers._base import ParameterParser

_numpydoc_sections = [
    # not super interested in these first three sections.
    # "Signature"
    # "Summary"
    # "Extended Summary"
    "Parameters",
    "Attributes",
    "Methods",
    "Returns",
    "Yields",
    "Receives",
    "Other Parameters",
    "Raises",
    "Warns",
    "Warnings",
    "See Also",
    "Notes",
    "References",
    "Examples",
    "index",
]
_section_regexs = []
for section in _numpydoc_sections:
    section_regex = rf"(?:(?:^|\n){section}\n-{{{len(section)}}}\n(?P<{section.lower().replace(' ', '_')}>[\s\S]*?))"
    _section_regexs.append(section_regex)

# These numpy regexes require a cleaned docstring
# first gets the contents of each section (assuming they are in order)
NUMPY_SECTION_REGEX = re.compile(
    rf"^(?P<summary>[\s\S]+?)??{'?'.join(_section_regexs)}?$"
)
# Next parses for "arg : type" items.
NUMPY_ARG_TYPE_REGEX = re.compile(
    r"^(?P<arg_name>\S.*?)(?:\s*:\s*(?P<type>.*?))?$", re.MULTILINE
)

# parse an 'arg' for multiple args
NUMPY_ARG_SPLIT_REGEX = re.compile(r"\s*,\s*")


def _pairwise(iterable):
    a, b = itertools.tee(iterable)
    next(b, None)
    return itertools.zip_longest(a, b, fillvalue=None)


class NumpydocParser(ParameterParser):

    @classmethod
    def doc_parameter_parser(cls, docstring: str) -> dict[str, tuple[Optional[str], Optional[str]]]:

        """Parse a numpydoc string for parameter descriptions.

        Parameters
        ----------
        docstring : str

        Returns
        -------
        dict[str, tuple[str|None, str|None]]
            A dictionary indexed by arguments found in the docstring pointing to their
             type descriptions and long descriptions.
        """
        docstring = inspect.cleandoc(docstring)
        doc_sections = NUMPY_SECTION_REGEX.search(docstring).groupdict()
        parameters = doc_sections.get("parameters")
        debug_level = get_debug_level()
        if parameters is None:
            if debug_level and "Parameters\n-" in docstring:
                raise DoceratorParsingError(
                    "Unable to parse docstring for parameters section, but it looks like there might be a "
                    "'Parameters' section. Did you not put the correct number of `-` on the line below it? "
                    "Are the sections in the correct order?",
                )
            parameters = ""

        others = doc_sections.get("other_parameters")
        if debug_level and others is None and "Other Parameters\n-" in docstring:
            raise DoceratorParsingError(
                "Unable to parse docstring for other parameters section, but it looks like there "
                "might be an `Other Parameters` section. Did you not put the correct number of `-` on the "
                "line below it? Are the sections in the correct order?",
            )
        if others:
            parameters += "\n" + others

        out_dict = {}
        matches = False
        for match, next_match in _pairwise(NUMPY_ARG_TYPE_REGEX.finditer(parameters)):
            matches = True
            arg, type_string = match.groups()

            if type_string == "":
                type_string = None
            # skip over values that are to be replaced (if there are any left)
            # and skip over *args, **kwargs arguments (but need the numpy arg
            # type regex to match them, so it pull the right descriptions).
            if not REPLACE_REGEX.match(arg) and arg[0] != "*":
                # +1 removes the newline character at the end of the argument : type_name match
                start = match.end() + 1
                # -1 removes the newline character at the start of the argument : type_name match (if there was one).
                end = next_match.start() - 1 if next_match is not None else None
                description = parameters[start:end]
                if description == "":
                    description = None
                # split these again, because multiple parameters can share the same
                # descriptions.
                for arg_part in NUMPY_ARG_SPLIT_REGEX.split(arg):
                    out_dict[arg_part] = (type_string, description)
        if debug_level and not matches and "Parameters\n-" in docstring:
            raise DoceratorParsingError(
                "Did not find any documented arguments in any parameter sections. "
                "Check the docstring formatting, ensuring the argument names are at the same indentation level as "
                "the section headings."
            )
        return out_dict


    @classmethod
    def format_parameter(cls, param: Union[DescribedParameter, list[DescribedParameter]]) -> str:
        # If param is an iterator, all parameters will have the same type description and long description
        if isinstance(param, DescribedParameter):
            param = [param]
        if len(param) == 0:
            raise ValueError("param cannot be an empty list.")
        formatted = ", ".join([par.name for par in param])
        if param[0].type_description is not None:
            formatted += f" : {param[0].type_description}"
        if param[0].long_description is not None:
            long_description = textwrap.indent(param[0].long_description, "    ")
            formatted += f"\n{long_description}"
        return formatted
