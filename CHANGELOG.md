# Changelog

## 0.6.3

- Route control requests to guarded mounted browser tabs through the page-target provider API.

## 0.6.2

- Expose the standalone window document as page target 0 through blitz-control-protocol ^0.7.2, with target discovery and identity checks for page control requests.

## 0.6.1

- Require ps-blitz ^0.6 and blitz-control-protocol ^0.7.1 so the runtime shares the strict TLS engine with its consumers.
- Build the CSS probe in the workspace using published renderer and engine dependencies.

## 0.6.0

- Serve inspection protocol 2 through blitz-control-protocol ^0.7.0, matching ps-qa 0.9.0 and Chuzz's direct protocol dependency.
- Add on_menu_override so an application can handle a standard menu item such as Close.
