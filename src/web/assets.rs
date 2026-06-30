pub(super) const INDEX_HTML: &str = include_str!("index.html");
const APP_CSS: &str = include_str!("app.css");
const APP_JS: &str = include_str!("app.js");
const LIB_API_JS: &str = include_str!("lib/api.js");
const LIB_CONSTANTS_JS: &str = include_str!("lib/constants.js");
const LIB_DOM_JS: &str = include_str!("lib/dom.js");
const LIB_PROJECTS_JS: &str = include_str!("lib/projects.js");
const LIB_STORE_JS: &str = include_str!("lib/store.js");
const COMPONENT_COMPOSER_JS: &str = include_str!("components/mu-composer.js");
const COMPONENT_CONVERSATION_JS: &str = include_str!("components/mu-conversation-view.js");
const COMPONENT_MODAL_JS: &str = include_str!("components/mu-project-modal.js");
const COMPONENT_SIDEBAR_JS: &str = include_str!("components/mu-sidebar.js");
const STYLE_BASE_CSS: &str = include_str!("styles/base.css");
const STYLE_COMPOSER_CSS: &str = include_str!("styles/composer.css");
const STYLE_CONVERSATION_CSS: &str = include_str!("styles/conversation.css");
const STYLE_LAYOUT_CSS: &str = include_str!("styles/layout.css");
const STYLE_MODAL_CSS: &str = include_str!("styles/modal.css");
const STYLE_SIDEBAR_CSS: &str = include_str!("styles/sidebar.css");
const STYLE_TOKENS_CSS: &str = include_str!("styles/tokens.css");

pub(super) struct StaticAsset {
    pub(super) content_type: &'static str,
    pub(super) body: &'static str,
}

const STATIC_ASSETS: &[(&str, StaticAsset)] = &[
    (
        "/app.css",
        StaticAsset {
            content_type: "text/css; charset=utf-8",
            body: APP_CSS,
        },
    ),
    (
        "/app.js",
        StaticAsset {
            content_type: "text/javascript; charset=utf-8",
            body: APP_JS,
        },
    ),
    (
        "/lib/api.js",
        StaticAsset {
            content_type: "text/javascript; charset=utf-8",
            body: LIB_API_JS,
        },
    ),
    (
        "/lib/constants.js",
        StaticAsset {
            content_type: "text/javascript; charset=utf-8",
            body: LIB_CONSTANTS_JS,
        },
    ),
    (
        "/lib/dom.js",
        StaticAsset {
            content_type: "text/javascript; charset=utf-8",
            body: LIB_DOM_JS,
        },
    ),
    (
        "/lib/projects.js",
        StaticAsset {
            content_type: "text/javascript; charset=utf-8",
            body: LIB_PROJECTS_JS,
        },
    ),
    (
        "/lib/store.js",
        StaticAsset {
            content_type: "text/javascript; charset=utf-8",
            body: LIB_STORE_JS,
        },
    ),
    (
        "/components/mu-composer.js",
        StaticAsset {
            content_type: "text/javascript; charset=utf-8",
            body: COMPONENT_COMPOSER_JS,
        },
    ),
    (
        "/components/mu-conversation-view.js",
        StaticAsset {
            content_type: "text/javascript; charset=utf-8",
            body: COMPONENT_CONVERSATION_JS,
        },
    ),
    (
        "/components/mu-project-modal.js",
        StaticAsset {
            content_type: "text/javascript; charset=utf-8",
            body: COMPONENT_MODAL_JS,
        },
    ),
    (
        "/components/mu-sidebar.js",
        StaticAsset {
            content_type: "text/javascript; charset=utf-8",
            body: COMPONENT_SIDEBAR_JS,
        },
    ),
    (
        "/styles/base.css",
        StaticAsset {
            content_type: "text/css; charset=utf-8",
            body: STYLE_BASE_CSS,
        },
    ),
    (
        "/styles/composer.css",
        StaticAsset {
            content_type: "text/css; charset=utf-8",
            body: STYLE_COMPOSER_CSS,
        },
    ),
    (
        "/styles/conversation.css",
        StaticAsset {
            content_type: "text/css; charset=utf-8",
            body: STYLE_CONVERSATION_CSS,
        },
    ),
    (
        "/styles/layout.css",
        StaticAsset {
            content_type: "text/css; charset=utf-8",
            body: STYLE_LAYOUT_CSS,
        },
    ),
    (
        "/styles/modal.css",
        StaticAsset {
            content_type: "text/css; charset=utf-8",
            body: STYLE_MODAL_CSS,
        },
    ),
    (
        "/styles/sidebar.css",
        StaticAsset {
            content_type: "text/css; charset=utf-8",
            body: STYLE_SIDEBAR_CSS,
        },
    ),
    (
        "/styles/tokens.css",
        StaticAsset {
            content_type: "text/css; charset=utf-8",
            body: STYLE_TOKENS_CSS,
        },
    ),
];

pub(super) fn static_asset(path: &str) -> Option<&'static StaticAsset> {
    STATIC_ASSETS
        .iter()
        .find_map(|(asset_path, asset)| (*asset_path == path).then_some(asset))
}
