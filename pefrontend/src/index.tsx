import { render, createRef, Component } from 'preact';
import * as monaco from 'monaco-editor';
import { signal } from '@preact/signals';
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

const images = signal({});
const chosenImage = signal(null);

function getOrCeateFile(path, text) {
    let uri = monaco.Uri.file(path);
    return monaco.editor.getModel(uri) || monaco.editor.createModel(text, undefined, uri);
}

class Editor extends Component {
    ref = createRef();
    editor: any;
    state = {
        models: [], // model.ITextModel[]
    };

    constructor({initialFiles}) {
        super();
        this.state.models = initialFiles.map(({name, data}) => getOrCeateFile(name, data));
    }

    componentDidMount() {
        this.editor = monaco.editor.create(this.ref.current, {
            placeholder: 'hello enter your text here',
        });
        if (this.state.models) {
            this.editFile(this.state.models[0].uri);
        }
    }

    editFile(uri) {
        this.editor.updateOptions({placeholder: ''});
        this.editor.setModel(monaco.editor.getModel(uri));
    }

    // this.props, this.state
    render({}, {models}) {
        let tabs = models.map((model) => {
            return (<button key={model.id} onClick={() => this.editFile(model.uri)}>{model.uri.path}</button>);
        });
        return <div>
            {tabs}
            <div class="editorContainer" ref={this.ref}></div>
        </div>;
    }
}

class ImagePicker extends Component {
    render() {
        const curImages = images.value;
        console.log('rendering imagePicker', curImages);
        let options = curImages.images?.map(({info,links}) => {
            let name = `${info.registry}/${info.repository}/${info.tag}`;
            return <option key={links.runi} value={links.runi}>{name}</option>;
        });
        console.log('rendering imagePicker', options);
        return (
            <select onChange={(event) => { chosenImage.value = event.currentTarget.value; }}>
            {options}
            </select>
        );
    }
}

class App extends Component {
    constructor({initialFiles}) {
        super();
    }

    // this.props, this.state
    render({initialFiles}, {}) {
        return <div>
          <ImagePicker />
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

const response = await fetch(window.location.origin + '/api/v1/images');
console.log(response);
if (response.ok) {
    const json = await response.json();
    images.value = json;
    console.log(json);
} else {
    console.log('oh no error');
}
//<Editor
//  height="80vh"
//  theme="vs-dark"
//  path={file.name}
//  defaultLanguage={file.language}
//  defaultValue={file.value}
///>
