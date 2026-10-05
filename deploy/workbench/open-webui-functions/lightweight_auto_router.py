"""Open WebUI Pipe: optional Jev advice with a local deterministic fallback.

Import this file through Admin Panel -> Functions. This Pipe uses Open WebUI's
normal completion path after choosing a configured Lightweight model, so native
RAG, web search, tool calls, and terminal controls continue to work.
"""

import json
import os
from typing import Any, Optional

import httpx
from pydantic import BaseModel, Field


ROUTES = {"direct", "documents", "web", "documents_and_web", "terminal"}


class Pipe:
    class Valves(BaseModel):
        priority: int = Field(default=0, description="Open WebUI function priority.")

    def __init__(self):
        self.valves = self.Valves()

    def pipes(self):
        return [{"id": "auto", "name": "Lightweight Auto"}]

    @staticmethod
    def _enabled() -> bool:
        return os.getenv("JEV_ENABLED", "false").strip().lower() in {"1", "true", "yes", "on"}

    @staticmethod
    def _last_user_prompt(body: dict, metadata: Optional[dict]) -> str:
        if isinstance(metadata, dict) and isinstance(metadata.get("user_prompt"), str):
            return metadata["user_prompt"]
        for message in reversed(body.get("messages", [])):
            if message.get("role") == "user":
                content = message.get("content", "")
                if isinstance(content, str):
                    return content
                if isinstance(content, list):
                    return "\n".join(
                        item.get("text", "") for item in content
                        if isinstance(item, dict) and item.get("type") == "text"
                    )
        return ""

    @staticmethod
    def _feature_flags(body: dict, metadata: Optional[dict]) -> dict:
        features = body.get("features", {})
        if not isinstance(features, dict) and isinstance(metadata, dict):
            features = metadata.get("features", {})
        if not isinstance(features, dict):
            features = {}
        files = body.get("files", [])
        if not files and isinstance(metadata, dict):
            files = metadata.get("files", [])
        return {
            "documents_attached": bool(files),
            "web_search_selected": bool(features.get("web_search")),
            "terminal_selected": bool(
                features.get("code_interpreter") or features.get("terminal")
            ),
        }

    @staticmethod
    def _allowlist() -> set[str]:
        return {
            model.strip()
            for model in os.getenv("LIGHTWEIGHT_AUTO_ALLOWED_MODELS", "").split(",")
            if model.strip()
        }

    @staticmethod
    def _route_models() -> dict[str, str]:
        try:
            value = json.loads(os.getenv("JEV_ROUTE_MODELS", "{}"))
        except json.JSONDecodeError:
            return {}
        return value if isinstance(value, dict) else {}

    @staticmethod
    def _default_model() -> str:
        return os.getenv("LIGHTWEIGHT_AUTO_DEFAULT_MODEL", "").strip()

    def _fallback(self) -> str:
        default = self._default_model()
        return default if default in self._allowlist() else ""

    async def _advice(self, prompt: str, flags: dict[str, bool]) -> Optional[tuple[str, float]]:
        # TYPESAFE_API_KEY is the documented deployment variable. The alias
        # keeps existing early workbench .env files working during upgrade.
        api_key = os.getenv("TYPESAFE_API_KEY", os.getenv("JEV_API_KEY", "")).strip()
        api_url = os.getenv("JEV_API_URL", "https://api.typesafe.ai/v1/systemone").strip()
        if not self._enabled() or not api_key or not api_url.startswith("https://"):
            return None
        try:
            limit = max(1, min(int(os.getenv("JEV_MAX_PROMPT_CHARS", "1200")), 4000))
            timeout = max(0.1, min(float(os.getenv("JEV_TIMEOUT_SECONDS", "1.5")), 10.0))
        except ValueError:
            return None
        payload = {
            "model": os.getenv("JEV_MODEL", "jev-latest"),
            "state": {"prompt": prompt[:limit], "features": flags},
            "questions": {
                "route": {
                    "type": "choice",
                    "instructions": "Choose the best agent-harness route for this turn.",
                    "criteria": {
                        "direct": "Answer without external retrieval or terminal work.",
                        "documents": "Use user-attached documents when available.",
                        "web": "Use web search for current external information when selected.",
                        "documents_and_web": "Use both attached documents and web search when selected.",
                        "terminal": "Use the isolated terminal only when it is selected and needed.",
                    },
                }
            },
        }
        try:
            async with httpx.AsyncClient(timeout=httpx.Timeout(timeout), follow_redirects=False) as client:
                response = await client.post(
                    api_url,
                    headers={"Authorization": f"Bearer {api_key}", "Content-Type": "application/json"},
                    json=payload,
                )
                response.raise_for_status()
                answer = response.json().get("answers", {}).get("route", {})
            route, confidence = answer.get("choice"), answer.get("confidence")
            if route not in ROUTES or isinstance(confidence, bool) or not isinstance(confidence, (int, float)):
                return None
            if not 0.0 <= float(confidence) <= 1.0:
                return None
            return route, float(confidence)
        except (httpx.HTTPError, ValueError, TypeError, KeyError):
            return None

    async def pipe(
        self,
        body: dict,
        __user__: Optional[dict] = None,
        __request__: Any = None,
        __metadata__: Optional[dict] = None,
        __event_emitter__=None,
    ):
        fallback = self._fallback()
        if not fallback:
            return "Lightweight Auto is not configured. Set an allowlisted LIGHTWEIGHT_AUTO_DEFAULT_MODEL."

        selected = fallback
        advice = await self._advice(self._last_user_prompt(body, __metadata__), self._feature_flags(body, __metadata__))
        if advice is not None:
            route, confidence = advice
            try:
                minimum = max(0.0, min(float(os.getenv("JEV_MIN_CONFIDENCE", "0.70")), 1.0))
            except ValueError:
                minimum = 1.0
            candidate = self._route_models().get(route, "")
            if confidence >= minimum and candidate in self._allowlist():
                selected = candidate

        if __event_emitter__:
            await __event_emitter__({
                "type": "status",
                "data": {"description": "Routing through Lightweight", "done": True, "hidden": True},
            })

        # This documented Open WebUI utility continues through its normal model
        # and tool orchestration instead of proxying directly to the gateway.
        from open_webui.models.users import Users
        from open_webui.utils.chat import generate_chat_completion

        if not __request__ or not __user__ or not __user__.get("id"):
            return "Lightweight Auto requires an authenticated Open WebUI chat session."
        user = await Users.get_user_by_id(__user__["id"])
        if user is None:
            return "Lightweight Auto could not resolve the Open WebUI user."
        payload = dict(body)
        payload["model"] = selected
        return await generate_chat_completion(__request__, payload, user)
