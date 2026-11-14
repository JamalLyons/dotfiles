# Update Fisher plugins (Homebrew updates are handled by Vorssaint)
# Usage: fisherup
function fisherup
    if not type -q fisher
        echo "Fisher not found"
        return 1
    end

    echo "→ fisher update"
    fisher update
    or return $status

    echo "✓ Fisher plugins updated"
end
