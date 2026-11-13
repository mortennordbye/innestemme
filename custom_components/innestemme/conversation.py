"""The conversation agent an Assist pipeline picks: every request goes to the engine."""

from __future__ import annotations

from typing import Literal

from homeassistant.components import conversation
from homeassistant.config_entries import ConfigEntry
from homeassistant.const import CONF_API_KEY, CONF_URL, MATCH_ALL
from homeassistant.core import HomeAssistant
from homeassistant.helpers import intent
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.helpers.device_registry import DeviceInfo
from homeassistant.helpers.entity_platform import AddConfigEntryEntitiesCallback

from .api import CannotConnect, InvalidToken, ask
from .const import DOMAIN


async def async_setup_entry(
    hass: HomeAssistant, entry: ConfigEntry, async_add_entities: AddConfigEntryEntitiesCallback
) -> None:
    async_add_entities([InnestemmeAgent(entry)])


class InnestemmeAgent(conversation.ConversationEntity):
    _attr_has_entity_name = True
    _attr_name = None

    def __init__(self, entry: ConfigEntry) -> None:
        self._entry = entry
        self._attr_unique_id = entry.entry_id
        self._attr_device_info = DeviceInfo(
            identifiers={(DOMAIN, entry.entry_id)}, name=entry.title, manufacturer="innestemme"
        )

    @property
    def supported_languages(self) -> list[str] | Literal["*"]:
        # The engine decides what it understands, English and Norwegian alike.
        return MATCH_ALL

    async def _async_handle_message(
        self, user_input: conversation.ConversationInput, chat_log: conversation.ChatLog
    ) -> conversation.ConversationResult:
        response = intent.IntentResponse(language=user_input.language)
        session = async_get_clientsession(self.hass)
        data = self._entry.data
        try:
            answer = await ask(session, data[CONF_URL], data[CONF_API_KEY], user_input.text)
        except InvalidToken:
            response.async_set_error(intent.IntentResponseErrorCode.UNKNOWN, "innestemme refused the token.")
        except CannotConnect:
            response.async_set_error(intent.IntentResponseErrorCode.UNKNOWN, "innestemme is not answering.")
        else:
            # An empty answer ends quietly in the room; Assist needs words, so it confirms instead.
            response.async_set_speech(answer or "Done.")
        return conversation.ConversationResult(response=response, conversation_id=chat_log.conversation_id)
