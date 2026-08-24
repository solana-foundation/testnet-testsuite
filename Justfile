set shell := ["zsh", "-cu"]

fmt:
    pnpm exec prettier --check .

lint:
    pnpm run check

bootstrap-tokens *ARGS:
    doppler run --project testnet-testsuite --config prd -- pnpm bootstrap:tokens {{ARGS}}

bootstrap-tokens-dev *ARGS:
    doppler run --project testnet-testsuite --config dev -- pnpm bootstrap:tokens:local {{ARGS}}
