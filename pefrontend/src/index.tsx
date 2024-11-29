import { render, createRef, Component } from 'preact';
import * as monaco from 'monaco-editor';
//import Editor from '@monaco-editor/react';


// https://github.com/vitejs/vite/discussions/1791#discussioncomment-9281911
import editorWorker from "monaco-editor/esm/vs/editor/editor.worker?worker";
import jsonWorker from "monaco-editor/esm/vs/language/json/json.worker?worker";
import cssWorker from "monaco-editor/esm/vs/language/css/css.worker?worker";
import htmlWorker from "monaco-editor/esm/vs/language/html/html.worker?worker";
import tsWorker from "monaco-editor/esm/vs/language/typescript/ts.worker?worker";

self.MonacoEnvironment = {
  getWorker(_: any, label: string) {
    if (label === "json") {
      return new jsonWorker();
    }
    if (label === "css" || label === "scss" || label === "less") {
      return new cssWorker();
    }
    if (label === "html" || label === "handlebars" || label === "razor") {
      return new htmlWorker();
    }
    if (label === "typescript" || label === "javascript") {
      return new tsWorker();
    }
    return new editorWorker();
  },
};

import './style.css';

type File = {
    name: string,
    data: string,
};

function getOrCeateFile(path, text) {
    let uri = monaco.Uri.file(path);
    return monaco.editor.getModel(uri) || monaco.editor.createModel(text, undefined, uri);
}

class Editor extends Component {
    //model: monaco.ITextModel; // todo figure out how to get this
    ref = createRef();
    editor: any;
    state = {
        models: [],
    };

    constructor({initialFiles}) {
        super();
        this.state.models = initialFiles.map(({name, data}) => getOrCeateFile(name, data));
    }

    componentDidMount() {
        this.editor = monaco.editor.create(this.ref.current, {
            placeholder: 'hello enter your text here',
        });
    }

    editFile(uri) {
        this.editor.updateOptions({placeholder: ''});
        this.editor.setModel(monaco.editor.getModel(uri));
    }

    // this.props, this.state
    render({}, {models}) {
        let tabs = models.map((model) => {
            console.log(model.uri);
            return (<button key={model.id} onClick={() => this.editFile(model.uri)}>{model.uri.path}</button>);
        });
        return <div>
            {tabs}
            <div class="editorContainer" ref={this.ref}></div>
        </div>;
    }
}

class App extends Component {
    constructor({initialFiles}) {
        super();
    }

    // this.props, this.state
    render({initialFiles}, {}) {
        return <div>
          <Editor
            initialFiles={initialFiles}
          />
        </div>
    }
}

const files: File[] = [
  {
    name: 'script.js',
    data: 'let a = 10;',
  },
  {
    name: 'style.css',
    data: 'body { border: 10px; }',
  },
  {
    name: 'index.html',
    data: '<div>hi</div>',
  },
];


// TODO something about the args going in not being compat with the Component type signature
render(<App initialFiles={files} />, document.getElementById('app'));
//<Editor
//  height="80vh"
//  theme="vs-dark"
//  path={file.name}
//  defaultLanguage={file.language}
//  defaultValue={file.value}
///>
