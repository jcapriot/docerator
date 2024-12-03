import re
import textwrap
from typing import Optional

import pytest

from docerator import DescribedParameter
from inspect import Parameter

DESCRIBED_PARAM_ATTRS = [
    "name",
    "kind",
    "default",
    "annotation",
    "type_description",
    "long_description",
]


def test_slot_assignment_guard():
    param = DescribedParameter('item', Parameter.POSITIONAL_OR_KEYWORD)
    with pytest.raises(AttributeError, match="'DescribedParameter' object has no attribute 'not_an_attribute'"):
        param.not_an_attribute = 1

def test_wrong_name_type():
    with pytest.raises(TypeError):
        DescribedParameter(5, Parameter.POSITIONAL_OR_KEYWORD)

def test_wrong_kind_arg():
    with pytest.raises(ValueError, match=r"'positional' is not a valid Parameter\.kind"):
        DescribedParameter('arg1', 'positional')

def test_keyword_only_input_error():
    with pytest.raises(TypeError, match=".*"+re.escape("__init__() takes 3 positional arguments but 4 were given")):
        DescribedParameter('arg1', Parameter.POSITIONAL_OR_KEYWORD, 2)

def test_default_passthrough():
    param = DescribedParameter('arg1', Parameter.POSITIONAL_OR_KEYWORD, default=2)
    assert param.default == 2

def test_annotation_passthrough():
    param = DescribedParameter('arg1', Parameter.POSITIONAL_OR_KEYWORD, annotation=Optional[str])
    assert param.annotation is Optional[str]

def test_bad_type_description_type():
    with pytest.raises(TypeError, match="type_description must be a str, not a int"):
        DescribedParameter('arg1', Parameter.POSITIONAL_OR_KEYWORD, type_description=5)

def test_bad_long_description_type():
    with pytest.raises(TypeError, match="long_description must be a str, not a int"):
        DescribedParameter('arg1', Parameter.POSITIONAL_OR_KEYWORD, long_description=5)

def test_type_description_default():
    param = DescribedParameter('arg1', Parameter.POSITIONAL_OR_KEYWORD)
    assert param.type_description is None

def test_long_description_default():
    param = DescribedParameter('arg1', Parameter.POSITIONAL_OR_KEYWORD)
    assert param.long_description is None

def test_good_type_description():
    param = DescribedParameter('arg1', Parameter.POSITIONAL_OR_KEYWORD, type_description='str')
    assert param.type_description == 'str'

def test_good_long_description():
    long_description = """
    I've got a really long, multiple line description, that will get dedented.
    """
    param = DescribedParameter('arg1', Parameter.POSITIONAL_OR_KEYWORD, long_description=long_description)
    assert param.long_description == textwrap.dedent(long_description)

def test_replace_returns_copy():
    param = DescribedParameter('arg1', Parameter.POSITIONAL_OR_KEYWORD)
    param1 = param.replace()
    assert param is not param1
    assert param == param1

@pytest.mark.parametrize(
    ["arg", "value"],
    [
        ['name', 'arg2'],
        ['kind', Parameter.KEYWORD_ONLY],
        ['annotation', bool],
        ['default', True],
        ['type_description', 'bool'],
        ['long_description', 'A boolean value'],
    ]
)
def test_replace(arg, value):
    param1 = DescribedParameter('arg1', Parameter.POSITIONAL_OR_KEYWORD)
    param2 = param1.replace(**{arg: value})
    for attr_name in DESCRIBED_PARAM_ATTRS:
        if attr_name == arg:
            assert getattr(param2, arg) == value
        else:
            assert getattr(param1, attr_name) == getattr(param2, attr_name)


def test_equality_self_object():
    param = DescribedParameter('arg1', Parameter.POSITIONAL_OR_KEYWORD)
    assert param == param

def test_equality():
    param1 = DescribedParameter('arg1', Parameter.POSITIONAL_OR_KEYWORD)
    param2 = DescribedParameter('arg1', Parameter.POSITIONAL_OR_KEYWORD)
    assert param1 == param2


def test_equality_with_parameter():
    param1 = Parameter('arg1', Parameter.POSITIONAL_OR_KEYWORD)
    param2 = DescribedParameter('arg1', Parameter.POSITIONAL_OR_KEYWORD, type_description='str')
    assert param2 == param1

def test_not_equal():
    param1 = DescribedParameter('arg1', Parameter.POSITIONAL_OR_KEYWORD)
    assert param1 != 5


def test_from_inspect_parameter():
    param1 = Parameter('arg1', Parameter.POSITIONAL_OR_KEYWORD)
    param2 = DescribedParameter.from_inspect_param(param1, type_description='str', long_description='A string!')
    assert param1 == param2
    assert param2.type_description == 'str'
    assert param2.long_description == 'A string!'


def test_hashing():
    param1 = DescribedParameter('arg1', Parameter.POSITIONAL_OR_KEYWORD, type_description='str', long_description='A string!')
    hash1 = param1.__hash__()

    param2 = param1.replace()
    hash2 = param2.__hash__()

    assert hash1 == hash2

    lookup = {param1:'found it!'}
    assert lookup[param2] == 'found it!'

