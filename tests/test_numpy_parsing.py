import inspect
from inspect import Parameter

import pytest

import docerator
import docerator._base as doc_base
import docerator.parsers._numpydoc as np_doc
from docerator._params import DescribedParameter


@pytest.fixture
def param_section():
    param_section = """
    item_no_type
    item1 : type
    item2_no_space: object, optional
    item3 :other no space
    item4
        I've got a 1 line description
    item5
        I've got a 2 line
        description
    item6 : type
        I've got a description line

        that has an empty line in it.
    item7 : type
        I've got a description line
        that ends with an empty line.

    multiple, args : shared type
        Shared Description

    %(replace.last_item)
    """
    param_section = inspect.cleandoc(param_section)
    return inspect.cleandoc(param_section)


@pytest.fixture
def param_references():
    # this is what the parser should give back.
    arg_names = [
        "item_no_type",
        "item1",
        "item2_no_space",
        "item3",
        "item4",
        "item5",
        "item6",
        "item7",
        "multiple",
        "args",
    ]

    arg_types = [
        None,
        "type",
        "object, optional",
        "other no space",
        None,
        None,
        "type",
        "type",
        "shared type",
        "shared type",
    ]
    arg_descriptions = [
        None,
        None,
        None,
        None,
        "    I've got a 1 line description",
        "    I've got a 2 line\n    description",
        "    I've got a description line\n\n    that has an empty line in it.",
        "    I've got a description line\n    that ends with an empty line.\n",
        "    Shared Description\n",
        "    Shared Description\n",
    ]
    out = {}
    for arg, arg_type, arg_description in zip(arg_names, arg_types, arg_descriptions):
        out[arg] = (arg_type, arg_description)

    return out


@pytest.mark.parametrize(
    "parsed_string, arg, arg_type",
    [
        ("item", "item", None),  # one argument, no type description
        (
            "multiple, items",
            "multiple, items",
            None,
        ),  # multiple arguments, no type description
        (
            "item : type",
            "item",
            "type",
        ),  # A well formatted argument, type description pair
        (
            "item: type",
            "item",
            "type",
        ),  # Same as above, but didn't include a space after the argument name
        ("item :type", "item", "type"),  # Didn't include a space after the ":"
        ("item:type", "item", "type"),  # No spaces around the ":"
        (
            "item : bool, default:True",
            "item",
            "bool, default:True",
        ),  # make sure it doesn't separate on the ":" in the type string.
        ("%(item.name)", "%(item.name)", None),
        ("*args", "*args", None),
        ("**kwargs", "**kwargs", None),
        (
            "    bad_type",
            None,
            None,
        ),  # shouldn't pick up on strings that start with a whitespace character.
    ],
)
def test_numpy_argtype_regex(parsed_string, arg, arg_type):
    match = np_doc.NUMPY_ARG_TYPE_REGEX.match(parsed_string)
    if match is None:
        assert arg is None and arg_type is None
    else:
        assert (arg, arg_type) == match.groups()


@pytest.mark.parametrize(
    "parsed_string, arg_names",
    [
        ("single_arg", ["single_arg"]),  # one argument
        ("two, args", ["two", "args"]),  # two arguments well formatted
        ("two,args", ["two", "args"]),  # two arguments, missing space after ","
        ("two ,args", ["two", "args"]),  # two arguments, space before ","
        ("two , args", ["two", "args"]),  # two arguments, space before and after ","
        ("has ,three, args", ["has", "three", "args"]),
    ],
)
def test_arg_split_regex(parsed_string, arg_names):
    assert np_doc.NUMPY_ARG_SPLIT_REGEX.split(parsed_string) == arg_names


def test_numpydoc_section_parsing():
    # Tests the section regex string that parses a clean numpydoc style doc string
    # into the different sections (The Regex is a little less strict than explicitly
    # following the numpydoc style).
    doc = """Summary
    Parameters
    ----------
    Hello

    Attributes
    ----------
    Item
    Other Parameters
    ----------------
    more parameters

    Raises
    ------
    A Warning
    Warns
    -----
    sends a warning

    Notes
    -----

    Examples
    --------
    item
    """
    # Sections must have at least 1 line in them (empty lines are valid for
    #     all but the last section).
    # Can be missing sections.
    # Sections need a blank line at the end.
    # Empty sections

    doc = inspect.cleandoc(doc)
    doc_sections = np_doc.NUMPY_SECTION_REGEX.search(doc).groupdict()

    reference_parse = {
        "summary": "Summary",
        "parameters": "Hello\n",
        "attributes": "Item",
        "methods": None,
        "returns": None,
        "yields": None,
        "receives": None,
        "other_parameters": "more parameters\n",
        "raises": "A Warning",
        "warns": "sends a warning\n",
        "warnings": None,
        "see_also": None,
        "notes": "",
        "references": None,
        "examples": "item",
        "index": None,
    }

    assert doc_sections == reference_parse


def test_numpy_argtype_regex_within_section_contents(param_section, param_references):
    # Create a parameter section as it would be output by the

    def tester(match):
        arg, type_string = match.groups()
        if doc_base.REPLACE_REGEX.match(arg):
            return False
        if arg[0] == "*":
            return False
        return True

    matches = list(
        filter(tester, np_doc.NUMPY_ARG_TYPE_REGEX.finditer(param_section))
    )
    assert len(matches) == 9  # should find the 9 matches
    for match in matches:
        args, type_string = match.groups()
        for arg in np_doc.NUMPY_ARG_SPLIT_REGEX.split(args):
            assert arg in param_references
            assert param_references[arg][0] == type_string


@pytest.mark.parametrize("section_split", ["parameters", "both"])
def test_parse_numpydoc_parameters(param_section, param_references, section_split):
    # parse the parameters inside a numpydoc style docstring:
    param_lines = param_section.splitlines()
    if section_split == "both":
        params, other_params = param_lines[:3], param_lines[3:]
    else:
        params = param_lines
        other_params = []
    params = "\n".join(params)
    other_params = "\n".join(other_params)
    # split the parameters into "parameters" and "other parameters"
    parameter_items = "\n    ".join(params.splitlines())
    other_parameter_items = "\n    ".join(other_params.splitlines())
    doc = f"""Summary

    With a bit of extended information. This is just fine.

    Parameters
    ----------
    {parameter_items}
    Methods
    -------
    func(a, b)
    """
    if other_params:
        doc = (
            doc
            + f"""
    Other Parameters
    ----------------
    {other_parameter_items}
    """
        )
    doc = (
        doc
        + """
    Examples
    --------
    Of doing something
    """
    )

    parsed = np_doc.NumpydocParser.doc_parameter_parser(doc)

    assert parsed == param_references


def test_parse_numpydoc_no_parameters():
    doc = """Summary
    Information about this class

    Returns
    -------
    nothing : None
        This doesn't return anything, But this description looks like an arg type
        It's just in the Returns section.
    """
    assert np_doc.NumpydocParser.doc_parameter_parser(doc) == {}


def test_class_npdoc_parsing():
    class TestClass:
        """Simple class with a docstring

        Parameters
        ----------
        item : object
            Could be anything really...
        a, b : float
            Two numbers to store on the class
        """

        def __init__(self, item, a, b): ...

    doc_dict = np_doc.NumpydocParser.parse_parameters(TestClass)

    verify_dict = dict(
        item=DescribedParameter(
            name="item",
            kind=Parameter.POSITIONAL_OR_KEYWORD,
            type_description="object",
            long_description='Could be anything really...'
        ),
        a=DescribedParameter(
            name="a",
            kind=Parameter.POSITIONAL_OR_KEYWORD,
            type_description="float",
            long_description='Two numbers to store on the class'
        ),
        b=DescribedParameter(
            name="b",
            kind=Parameter.POSITIONAL_OR_KEYWORD,
            type_description="float",
            long_description='Two numbers to store on the class'
        )
    )
    assert verify_dict == doc_dict


@pytest.mark.parametrize("dash_length", [3, 50])
@pytest.mark.parametrize("section", ["Parameters", "Other Parameters"])
@pytest.mark.parametrize("debug", [0, 1])
def test_parse_numpydoc_hyphen_errors(section, dash_length, debug):
    docerator.set_debug_level(debug)
    docstring = "Summary\n"
    if section != "Parameters":
        docstring += "\nParameters\n----------\nthing\n"
    docstring += f"\n{section}\n{'-'*dash_length}\nitem"

    if debug:
        match = f"(Unable to parse docstring for {section.lower()}).*"
        with pytest.raises(docerator.DoceratorParsingError, match=match):
            np_doc.NumpydocParser.doc_parameter_parser(docstring)
    else:
        # Shouldn't throw if no debug
        np_doc.NumpydocParser.doc_parameter_parser(docstring)
    docerator.set_debug_level(0)


@pytest.mark.parametrize("debug", [0, 1])
def test_bad_section_order(debug):
    docstring = """Summary

    Attributes
    ----------
    item1

    Parameters
    ----------
    item2

    Returns
    -------
    """

    docerator.set_debug_level(debug)
    if debug:
        match = "(Unable to parse docstring for parameters).*"
        with pytest.raises(docerator.DoceratorParsingError, match=match):
            np_doc.NumpydocParser.doc_parameter_parser(docstring)
    else:
        np_doc.NumpydocParser.doc_parameter_parser(docstring)

    docerator.set_debug_level(0)


@pytest.mark.parametrize("debug", [0, 1])
def test_bad_section_indent(debug):
    docstring = """Summary
     Parameters
    ----------
    item2

    Returns
    -------
    """

    docerator.set_debug_level(debug)
    if debug:
        match = "(Unable to parse docstring for parameters).*"
        with pytest.raises(docerator.DoceratorParsingError, match=match):
            np_doc.NumpydocParser.doc_parameter_parser(docstring)
    else:
        # Shouldn't throw if no debug
        assert np_doc.NumpydocParser.doc_parameter_parser(docstring) == {}

    docerator.set_debug_level(0)


@pytest.mark.parametrize("debug", [0, 1])
def test_unparseable_parameters(debug):
    docstring = """Summary
    Parameters
    ----------
     bad_indent
    """
    match = "(Did not find any documented arguments in any parameter).*"

    docerator.set_debug_level(debug)
    if debug:
        with pytest.raises(docerator.DoceratorParsingError, match=match):
            np_doc.NumpydocParser.doc_parameter_parser(docstring)
    else:
        # Shouldn't throw if no debug
        assert np_doc.NumpydocParser.doc_parameter_parser(docstring) == {}
    docerator.set_debug_level(0)


@pytest.mark.parametrize("debug", [0, 1])
def test_class_doc_with_extra_arg(debug):
    class TestClass:
        """Simple class with a docstring

        Parameters
        ----------
        item : object
            Could be anything really...
        a, b : float
            Two numbers to store on the class
        not_in_signature : object
            This argument is not explicitly in this class's call signature.
        """

        def __init__(self, item, a, b, **kwargs): ...

    docerator.set_debug_level(debug)
    if debug:
        match = "Documented argument not_in_signature is not in the signature of TestClass"
        with pytest.raises(docerator.DoceratorParsingError, match=match):
            np_doc.NumpydocParser.parse_parameters(TestClass)
    else:
        verify_dict = dict(
            item=DescribedParameter(
                name="item",
                kind=Parameter.POSITIONAL_OR_KEYWORD,
                type_description="object",
                long_description='Could be anything really...'
            ),
            a=DescribedParameter(
                name="a",
                kind=Parameter.POSITIONAL_OR_KEYWORD,
                type_description="float",
                long_description='Two numbers to store on the class'
            ),
            b=DescribedParameter(
                name="b",
                kind=Parameter.POSITIONAL_OR_KEYWORD,
                type_description="float",
                long_description='Two numbers to store on the class'
            ),
            not_in_signature=DescribedParameter(
                name="not_in_signature",
                kind=Parameter.KEYWORD_ONLY,
                default=None,
                type_description="object",
                long_description="This argument is not explicitly in this class's call signature."
            )
        )
        # Shouldn't throw if no debug
        assert np_doc.NumpydocParser.parse_parameters(TestClass) == verify_dict
    docerator.set_debug_level(0)
