import pytest
import docerator
import docerator._base as doc_base


@pytest.mark.parametrize(
    "string, target",
    [
        ("hello %(item)", "item"),
        ("I want to get everything in \n %(super.*)", "super.*"),
        ("%(item.key.TargetClass) is what I want.", "item.key.TargetClass"),
        ("%No match", None),
        ("%(should not match over \n multiple lines)", None),
        ("%(target.one, target.two)", "target.one, target.two"),
    ],
)
def test_replace_regex(string, target):
    search = doc_base.REPLACE_REGEX.search(string)
    if search is None:
        assert search is target
    else:
        assert search.group("replace_key") == target