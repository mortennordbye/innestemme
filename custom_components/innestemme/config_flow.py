"""Setup: the engine's URL and its ask-token, checked with one harmless request."""

from __future__ import annotations

from typing import Any

import voluptuous as vol

from homeassistant.config_entries import ConfigFlow, ConfigFlowResult
from homeassistant.const import CONF_API_KEY, CONF_URL
from homeassistant.helpers.aiohttp_client import async_get_clientsession

from .api import CannotConnect, InvalidToken, ask
from .const import DOMAIN

SCHEMA = vol.Schema({vol.Required(CONF_URL): str, vol.Required(CONF_API_KEY): str})


class InnestemmeConfigFlow(ConfigFlow, domain=DOMAIN):
    VERSION = 1

    async def async_step_user(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        errors: dict[str, str] = {}
        if user_input is not None:
            url = user_input[CONF_URL].strip().rstrip("/")
            token = user_input[CONF_API_KEY].strip()
            self._async_abort_entries_match({CONF_URL: url})
            try:
                # Reads only: it checks the URL and the token without switching anything.
                await ask(async_get_clientsession(self.hass), url, token, "what time is it")
            except InvalidToken:
                errors["base"] = "invalid_auth"
            except CannotConnect:
                errors["base"] = "cannot_connect"
            else:
                return self.async_create_entry(title="innestemme", data={CONF_URL: url, CONF_API_KEY: token})
        return self.async_show_form(
            step_id="user", data_schema=self.add_suggested_values_to_schema(SCHEMA, user_input), errors=errors
        )
