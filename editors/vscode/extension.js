const vscode = require('vscode');

let client = null;

function activate(context) {
    const config = vscode.workspace.getConfiguration('alloy');
    const executablePath = config.get('executablePath') || 'alloy';

    startLanguageClient(context, executablePath);

    const restartCmd = vscode.commands.registerCommand('alloy.restartLsp', async () => {
        if (client) {
            await client.stop();
            client = null;
        }
        startLanguageClient(context, executablePath);
        vscode.window.showInformationMessage('Alloy Language Server restarted.');
    });

    context.subscriptions.push(restartCmd);
}

function startLanguageClient(context, command) {
    try {
        const { LanguageClient, TransportKind } = require('vscode-languageclient/node');
        const serverOptions = {
            run: { command, args: ['lsp'], transport: TransportKind.stdio },
            debug: { command, args: ['lsp'], transport: TransportKind.stdio }
        };

        const clientOptions = {
            documentSelector: [{ scheme: 'file', language: 'alloy' }],
            synchronize: {
                fileEvents: vscode.workspace.createFileSystemWatcher('**/*.ajs')
            }
        };

        client = new LanguageClient('alloyLanguageServer', 'Alloy Language Server', serverOptions, clientOptions);
        client.start();
        context.subscriptions.push({ dispose: () => client && client.stop() });
    } catch (err) {
        console.log('vscode-languageclient not present or failed to load:', err);
    }
}

function deactivate() {
    if (client) {
        return client.stop();
    }
}

module.exports = {
    activate,
    deactivate
};
