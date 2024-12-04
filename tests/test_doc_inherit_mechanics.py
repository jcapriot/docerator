import re

import pytest

from docerator import set_debug_level
from docerator.doc_inherit import _import_target
import docerator

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
