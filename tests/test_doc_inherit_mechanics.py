import re

import pytest

from docerator import set_debug_level
from docerator.doc_inherit import _import_target
import docerator

@pytest.fixture()
def raise_debug_level():
    set_debug_level(1)
    yield
    set_debug_level(0)

def test_bad_import_targets(raise_debug_level):
    not_a_module_attribute = "bad_mod_attr"
    msg = re.escape("bad_mod_attr does not include the module information. Should be included as module.to.import.from.bad_mod_attr")
    with pytest.raises(ValueError, match=msg):
        _import_target(not_a_module_attribute)

def test_bad_import_attribute(raise_debug_level):
    not_importable_target = "inspect.bam"
    msg = re.escape("module inspect does not have an attribute named bam")
    with pytest.raises(AttributeError, match=msg):
        _import_target(not_importable_target)

def test_bad_class_member_import(raise_debug_level):
    not_importable_target = "docerator.NotClass.anything"
    msg = re.escape("Unable to import docerator.NotClass.anything for docstring replacement")
    with pytest.raises(ImportError, match=msg):
        _import_target(not_importable_target)


def test_class_bad_member_import(raise_debug_level):
    target = "docerator.DoceratorMeta.not_a_thing"
    msg = re.escape('Unable to import docerator.DoceratorMeta.not_a_thing for docstring replacement')
    with pytest.raises(ImportError, match=msg):
        _import_target(target)


def test_bad_import():
    pass
