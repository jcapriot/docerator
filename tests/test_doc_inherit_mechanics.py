import re
import inspect
from inspect import Parameter
import pytest

from docerator import set_debug_level, DescribedParameter, bind_signature_to_function
from docerator.doc_inherit import _import_target
import docerator

from docerator_testing_utils import py313_docstrip

@pytest.fixture(scope="function")
def debug_level(request):
    level = request.param
    set_debug_level(level)
    yield level
    set_debug_level(0)

@pytest.mark.parametrize(
    ['target', 'error_class', 'match'],
    [
        ["bad_mod_attr", ValueError, re.escape("bad_mod_attr does not include module information. Should be included as module.to.import.from.bad_mod_attr")],
        ["not_a_module.function", ImportError, re.escape("Unable to import not_a_module requested for documentation replacement")],
        ["inspect.bam", AttributeError, re.escape("inspect does not have the attribute named bam requested for documentation replacement")],
        ["not_a_module.Class.function", ImportError, re.escape("Unable to import not_a_module requested for documentation replacement")],
        ["docerator.NotClass.anything", AttributeError, re.escape("docerator does not have the attribute named NotClass requested for documentation replacement")],
        ["docerator.DoceratorMeta.not_a_thing", AttributeError, re.escape('docerator.DoceratorMeta does not have the attribute named not_a_thing requested for documentation replacement')],
    ]
)
@pytest.mark.parametrize('debug_level', [0, 1], indirect=True)
def test_import_errors(target, error_class, match, debug_level):
    if debug_level:
        with pytest.raises(error_class, match=match):
            _import_target(target)
    else:
        _import_target(target)

@pytest.mark.parametrize(
    ['target', 'valid_target'], [
        ["docerator.DescribedParameter", docerator.DescribedParameter],
        ["docerator.DescribedParameter.type_description",docerator.DescribedParameter.type_description],
        ["docerator.bind_signature_to_function", docerator.bind_signature_to_function],
    ]
)
def test_good_import(target, valid_target):
    imported = _import_target(target)
    assert imported is valid_target



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