#ifndef MAINWINDOW_H
#define MAINWINDOW_H

#include <QMainWindow>
#include <QLabel>
#include "imagecontroller.h"

QT_BEGIN_NAMESPACE
namespace Ui { class MainWindow; }
QT_END_NAMESPACE

class MainWindow : public QMainWindow
{
    Q_OBJECT

public:
    MainWindow(QWidget *parent, ImageController *controller);
    ~MainWindow();

protected:
    void keyPressEvent(QKeyEvent *event) override;
    void wheelEvent(QWheelEvent *event) override;
    void mousePressEvent(QMouseEvent *event) override;
    void mouseMoveEvent(QMouseEvent *event) override;
    void mouseReleaseEvent(QMouseEvent *event) override;
    void mouseDoubleClickEvent(QMouseEvent *event) override;


private:
    Ui::MainWindow *ui;
    QLabel *imageLabel;
    ImageController *imageController;  // Pointer to the controller
    double currentScale = 1.0;
    bool isPanning = false;           // To track if the user is panning
    QPoint lastMousePosition;         // To store the last mouse position
};

#endif // MAINWINDOW_H
