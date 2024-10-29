import inspect
import pathlib
import textwrap
from inspect import Parameter
import pytest
import docerator
import sys
import importlib.util

from docerator import bind_signature_to_function
from docerator._params import DescribedParameter
from docerator.parsers import NumpydocParser
import docerator.doc_inherit as doc_inherit


def import_from_path(module_name, file_path):
    spec = importlib.util.spec_from_file_location(module_name, file_path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[module_name] = module
    spec.loader.exec_module(module)
    return module

py313 = sys.version_info >= (3, 13)

def py313_docstrip(text):
    text=text.split("\n", maxsplit=1)
    return "\n".join([text[0].strip(), textwrap.dedent(text[1])])

numpydoc_classes = import_from_path('numpydoc', pathlib.Path(__file__).parent / "numpydoc_classes.py")
Parent = numpydoc_classes.Parent
ChildClass = numpydoc_classes.ChildClass
GrandchildClass = numpydoc_classes.GrandchildClass
CousinClass = numpydoc_classes.CousinClass


@pytest.mark.parametrize("args", [[], ["single"], ["two", "args"]])
def test_doc_replace(args):
    docstring = """A docstring
    Parameters
    ----------
    %(replace.me)
    """
    params = [
        DescribedParameter(
            name=arg,
            kind=inspect.Parameter.POSITIONAL_OR_KEYWORD,
            type_description="object",
            long_description="Description\nand more.",
        )
        for arg in args
    ]

    verified = f"""A docstring
    Parameters
    ----------
    {', '.join(args)} : object
        Description
        and more.
    """

    if args:
        formatted = NumpydocParser.format_parameter(params)
        assert (
            doc_inherit._replace_doc_args("replace.me", formatted, docstring)
            == verified
        )
    else:
        with pytest.raises(ValueError, match="param cannot be an empty list."):
            formatted = NumpydocParser.format_parameter(params)

def test_do_nothing():
    class Undocced(metaclass=docerator.DoceratorMeta):
        def __init__(self): ...

    assert Undocced.__doc__ is None


def test_nothing_to_insert():
    docstring = """A docstring

    Parameters
    ----------
    arg1 : object
        Extended Description.
    arg2 : int
        2 Extended Description.
    arg3 : int
        3 Extended Description.

    Other Parameters
    ----------------
    even_more : list
    but_not_too_much
        But another description.
    """

    init_sig = inspect.Signature(
        parameters=[
            Parameter(name="self", kind=Parameter.POSITIONAL_OR_KEYWORD),
            Parameter(name="arg1", kind=Parameter.POSITIONAL_OR_KEYWORD),
            Parameter(name="arg2", kind=Parameter.POSITIONAL_OR_KEYWORD),
            Parameter(name="arg3", kind=Parameter.POSITIONAL_OR_KEYWORD),
            Parameter(name="even_more", kind=Parameter.POSITIONAL_OR_KEYWORD),
            Parameter(name="but_not_too_much", kind=Parameter.POSITIONAL_OR_KEYWORD),
        ]
    )

    if py313:
        docstring = py313_docstrip(docstring)
    assert (Parent.__doc__, inspect.signature(Parent.__init__)) == (docstring, init_sig)


def test_child_docerator_meta():
    docstring = """Docstring

    Parameters
    ----------
    arg1 : int
        Not quite the same as parent
    a_new_arg : dict
        A dictionary.
    arg2 : int
        2 Extended Description.

    Other Parameters
    ----------------
    arg3 : int
        3 Extended Description.

    even_more : list
    but_not_too_much
        But another description.
    """

    init_sig = inspect.Signature(
        parameters=[
            Parameter(name="self", kind=Parameter.POSITIONAL_OR_KEYWORD),
            Parameter(name="arg1", kind=Parameter.POSITIONAL_OR_KEYWORD),
            Parameter(name="a_new_arg", kind=Parameter.POSITIONAL_OR_KEYWORD),
            DescribedParameter(
                name="arg2",
                kind=Parameter.KEYWORD_ONLY,
                type_description="int",
                long_description="2 Extended Description."
            ),
            DescribedParameter(
                name="arg3",
                kind=Parameter.KEYWORD_ONLY,
                type_description="int",
                long_description="3 Extended Description.\n"
            ),
            DescribedParameter(
                name="even_more",
                kind=Parameter.KEYWORD_ONLY,
                type_description="list",
            ),
            DescribedParameter(
                name="but_not_too_much",
                kind=Parameter.KEYWORD_ONLY,
                long_description="But another description."
            ),
        ]
    )

    if py313:
        docstring = py313_docstrip(docstring)

    assert (ChildClass.__doc__, inspect.signature(ChildClass.__init__)) == (
        docstring,
        init_sig,
    )


def test_grandchild_docerator_meta():
    docstring = """Docstring

    Parameters
    ----------
    arg1 : object
        Extended Description.
    a_new_arg : dict
        Still a dictionary...
    arg2 : int
        2 Extended Description.

    Other Parameters
    ----------------
    arg3 : int
        3 Extended Description.

    even_more : list
    """

    if py313:
        docstring = py313_docstrip(docstring)

    init_sig = inspect.Signature(
        parameters=[
            Parameter(name="self", kind=Parameter.POSITIONAL_OR_KEYWORD),
            DescribedParameter(
                name="arg1",
                kind=Parameter.POSITIONAL_OR_KEYWORD,
                type_description="object",
                long_description="Extended Description.",
            ),
            Parameter(name="a_new_arg", kind=Parameter.POSITIONAL_OR_KEYWORD),
            DescribedParameter(
                name="arg2",
                kind=Parameter.KEYWORD_ONLY,
                type_description="int",
                long_description="2 Extended Description.",
            ),
            DescribedParameter(
                name="arg3",
                kind=Parameter.KEYWORD_ONLY,
                type_description="int",
                long_description="3 Extended Description.\n",
            ),
            DescribedParameter(
                name="even_more",
                kind=Parameter.KEYWORD_ONLY,
                type_description="list",
            ),
            # This class won't end up with a **kwargs parameter
            # because it's parent didn't have it in it's processed
            # signature.
        ]
    )

    assert GrandchildClass.__doc__ == docstring
    assert GrandchildClass.__init__ is not ChildClass.__init__
    assert inspect.signature(GrandchildClass.__init__) == init_sig


def test_cousin_docerator_meta():
    init_string = """This is where I am created.

        Parameters
        ----------
        arg1 : int
            Not quite the same as parent
        a_new_arg : dict
            A dictionary.
        arg2 : int
            2 Extended Description.
        arg3 : int
            3 Extended Description.

        even_more : list
        but_not_too_much
            But another description.
        """

    if py313:
        init_string = py313_docstrip(init_string)

    init_sig = inspect.Signature(
        parameters=[
            Parameter(name="self", kind=Parameter.POSITIONAL_OR_KEYWORD),
            DescribedParameter(
                name="arg1",
                kind=Parameter.POSITIONAL_OR_KEYWORD,
                type_description="int",
                long_description="Not quite the same as parent",
            ),
            Parameter(name="a_new_arg", kind=Parameter.POSITIONAL_OR_KEYWORD),
            DescribedParameter(
                name="arg2",
                kind=Parameter.KEYWORD_ONLY,
                type_description="int",
                long_description="2 Extended Description.",
            ),
            DescribedParameter(
                name="arg3",
                kind=Parameter.KEYWORD_ONLY,
                type_description="int",
                long_description="3 Extended Description.\n",
            ),
            DescribedParameter(
                name="even_more",
                kind=Parameter.KEYWORD_ONLY,
                type_description="list",
            ),
            DescribedParameter(
                name="but_not_too_much",
                kind=Parameter.KEYWORD_ONLY,
                long_description="But another description."
            ),
        ]
    )

    assert CousinClass.__init__.__doc__ == init_string
    assert inspect.signature(CousinClass.__init__) == init_sig


def test_cousin_method_replace():
    func_string = """Return something

        Parameters
        ----------
        x : float
            The float
        whats_this : str
            The string.

        Returns
        -------
        b : str
             The output
        """

    if py313:
        func_string = py313_docstrip(func_string)

    func_sig = inspect.Signature(
        parameters=[
            Parameter(name="self", kind=Parameter.POSITIONAL_OR_KEYWORD),
            DescribedParameter(
                name="x",
                annotation=float,
                kind=Parameter.POSITIONAL_OR_KEYWORD,
                type_description="float",
                long_description="The float",
            ),
            DescribedParameter(
                name="whats_this",
                kind=Parameter.POSITIONAL_OR_KEYWORD,
                annotation=str,
                type_description="str",
                long_description="The string.",
            ),
        ]
    )

    assert CousinClass.a_function.__doc__ == func_string
    assert inspect.signature(CousinClass.a_function) == func_sig


def test_cousin_another_method_replace():
    func_string = """
        This wasn't any good...

        Parameters
        ----------
        whats_this : str
            String to query?
        its_nothing : bool, optional
            Is `whats_this` nothing?
        or_isit : bool, optional
            It is actualy something

        Returns
        -------
        bool
            I'm returning
        """

    if py313:
        func_string = py313_docstrip(func_string)

    func_sig = inspect.Signature(
        parameters=[
            Parameter(name="self", kind=Parameter.POSITIONAL_OR_KEYWORD),
            DescribedParameter(
                name="whats_this",
                kind=Parameter.POSITIONAL_OR_KEYWORD,
                annotation=str,
                type_description="str",
                long_description="String to query?",
            ),
            DescribedParameter(
                name="its_nothing",
                kind=Parameter.POSITIONAL_OR_KEYWORD,
                default=None,
                type_description="bool, optional",
                long_description="Is `whats_this` nothing?",
            ),
            Parameter(
                name="or_isit",
                kind=Parameter.POSITIONAL_OR_KEYWORD,
                default=False,
            ),
        ]
    )

    assert CousinClass.another_func.__doc__ == func_string
    assert inspect.signature(CousinClass.another_func) == func_sig

@pytest.mark.parametrize('update_signature', [True, False])
def test_func_wrapper(update_signature):
    @docerator.doc_wrap(update_signature=update_signature)
    def npdoc_function(whats_this):
        """I'm going to grab my parameter description

        Parameters
        ----------
        %(numpydoc_classes.Parent.a_function.whats_this)
        """

    docstring = """I'm going to grab my parameter description

        Parameters
        ----------
        whats_this : str
            The string.
        """

    if py313:
        docstring = py313_docstrip(docstring)

    assert npdoc_function.__doc__ == docstring

    new_sig = inspect.Signature(
        [DescribedParameter(
            name="whats_this",
            kind=Parameter.POSITIONAL_OR_KEYWORD,
            annotation=str,
            type_description="str",
            long_description="The string.",
        )]
    )

    assert npdoc_function.__name__ == 'npdoc_function'

    if update_signature:
        assert inspect.signature(npdoc_function) == new_sig
    else:
        assert new_sig != inspect.signature(npdoc_function)


def test_bind_signature():
    def func(x, y, *args, **kwargs):
        args = [x, y, *args]
        return args, kwargs

    out = func(1, 2, 5, 23, f=10, m=20)
    assert out[0] == [1, 2, 5, 23] and out[1] == {'f': 10, 'm': 20}

    new_sig = inspect.Signature(
        [
            DescribedParameter(name='x', kind=Parameter.POSITIONAL_OR_KEYWORD),
            DescribedParameter(name='y', kind=Parameter.POSITIONAL_OR_KEYWORD),
            DescribedParameter(name='z', kind=Parameter.POSITIONAL_OR_KEYWORD),
            DescribedParameter(name='a', kind=Parameter.KEYWORD_ONLY),
        ]
    )
    wrapped_func = bind_signature_to_function(new_sig, func)

    out = wrapped_func(1, 2, 3, a=10)

    assert out[0] == [1, 2, 3] and out[1] == {'a': 10}

    out = wrapped_func(1, 2, z=3, a=10)

    assert out[0] == [1, 2, 3] and out[1] == {'a': 10}

    out = wrapped_func(1, 2, a=10, z=3)

    assert out[0] == [1, 2, 3] and out[1] == {'a': 10}

    out = wrapped_func(y=2, x=1, a=10, z=3)

    assert out[0] == [1, 2, 3] and out[1] == {'a': 10}

    with pytest.raises(TypeError, match=".*missing a required argument: 'x'"):
        wrapped_func()

    with pytest.raises(TypeError, match=".*missing a required argument: 'y'"):
        wrapped_func(1)

    with pytest.raises(TypeError, match=".*missing a required argument: 'z'"):
        wrapped_func(1, 2)


