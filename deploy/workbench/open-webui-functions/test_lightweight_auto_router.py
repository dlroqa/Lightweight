"""Dependency-free policy tests for the Lightweight Auto Open WebUI Pipe."""

import asyncio
import importlib.util
import os
import sys
import types
from pathlib import Path


httpx = types.ModuleType("httpx")
httpx.HTTPError = Exception
httpx.Timeout = lambda value: value
httpx.AsyncClient = object
sys.modules["httpx"] = httpx

pydantic = types.ModuleType("pydantic")


class BaseModel:
    def __init__(self, **values):
        for key, value in values.items():
            setattr(self, key, value)


def Field(default=None, **_kwargs):
    return default


pydantic.BaseModel = BaseModel
pydantic.Field = Field
sys.modules["pydantic"] = pydantic

source = Path(__file__).with_name("lightweight_auto_router.py")
spec = importlib.util.spec_from_file_location("lightweight_auto_router", source)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
pipe = module.Pipe()

os.environ.update(
    {
        "LIGHTWEIGHT_AUTO_DEFAULT_MODEL": "fast",
        "LIGHTWEIGHT_AUTO_ALLOWED_MODELS": "fast,context",
        "JEV_ROUTE_MODELS": '{"documents":"context","direct":"outside"}',
        "JEV_ENABLED": "false",
    }
)

assert pipe._fallback() == "fast"
assert pipe._route_models()["documents"] == "context"
assert asyncio.run(pipe._advice("private prompt", {})) is None
assert pipe._last_user_prompt({"messages": [{"role": "user", "content": "hello"}]}, None) == "hello"
assert pipe._feature_flags({"files": [{}], "features": {"web_search": True}}, None) == {
    "documents_attached": True,
    "web_search_selected": True,
    "terminal_selected": False,
}
print("Lightweight Auto policy tests passed")
